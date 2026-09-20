//! User-Agent Server.
//!
//! What the UAS answers today:
//! - `OPTIONS` → `200 OK`.
//! - `INVITE` with SDP body → `100 Trying`, then `200 OK` carrying an
//!   SDP answer with an engine-allocated UDP port. Creates an early
//!   dialog; `ACK` confirms; `BYE` tears it down with `200 OK`.
//! - `INVITE` with no common codec → `488 Not Acceptable Here`.
//! - In-dialog re-`INVITE` / `UPDATE` → offer/answer is re-run
//!   against the dialog's existing media endpoint (codec change,
//!   hold via `sendonly` / `inactive`); out-of-order `CSeq` → `500`.
//! - `CANCEL` (RFC 3261 §9.2) → `200 OK` plus `487 Request
//!   Terminated` on a still-pending INVITE, `200 OK` with no effect
//!   once the INVITE has a final response, `481` otherwise.
//! - RFC 4028 session timers: `Session-Expires` / `Min-SE` /
//!   `Supported: timer` are honoured, the 2xx carries
//!   `Session-Expires` + `Require: timer`, re-INVITE / UPDATE refresh
//!   the timer, and a dialog nobody refreshes is ended with a BYE.
//! - **Rendezvous bridging**: two `INVITE`s with the same Request-URI
//!   user-part (e.g. both to `sip:room-1@engine`) are paired. The engine
//!   spins up a byte-transparent UDP bridge between their media sockets
//!   and tears it down on `BYE` from either side; the surviving leg
//!   receives an engine-originated BYE built from the dialog's route
//!   set, remote target, and local `CSeq`.
//! - Every other method → `405 Method Not Allowed`.
//!
//! Transaction layer: every request runs through the RFC 3261 §17
//! server FSMs in [`TransactionDriver`] (`ServerInviteTxn` with
//! timers G/H/I, `ServerNonInviteTxn` with timer J), so retransmits
//! replay the cached response byte-for-byte. INVITE 2xx bypasses the
//! FSM per §17.2.1; its retransmission is a per-dialog timer loop per
//! §13.3.1.4 (bytes parked on the [`DialogRecord`], driven by
//! [`UasServer::spawn_invite_2xx_retransmit`], cancelled on ACK). When
//! that loop exhausts 64·T1 without an ACK the dialog is terminated
//! with a BYE. A dialog also ends when a configured absolute maximum
//! call duration elapses.
//!
//! Concurrency: the ingress loop only parses, runs the transaction
//! layer, and dispatches; the Transaction-User work for each `Call-ID`
//! runs on its own ordered worker so a slow credential store (auth
//! lookups run on the blocking pool) cannot stall unrelated calls.
//!
//! Not implemented: N-party conferences beyond the conference-room
//! orchestrator seam, T.38 re-INVITE handling, and acting as the RFC
//! 4028 refresher (a peer that insists on `refresher=uas` gets no
//! session timer).

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
    ClientNonInviteTxn, DialogEvent, DialogFsm, Role as TxnRole, ServerInviteTxn,
    ServerNonInviteTxn, T1, T2, TIMEOUT_64T1, TransactionDriver, TransactionKey as TxnKey,
    TransactionState, TuEvent,
};

/// How long an idle per-`Call-ID` worker lingers before it retires.
/// Long enough that the INVITE → ACK → BYE cadence of a normal call
/// reuses one task; short enough that OPTIONS pings don't pile up
/// idle tasks.
const WORKER_IDLE: Duration = Duration::from_secs(2);

/// RFC 4028 §10: the side that is not the refresher sends BYE once
/// the session interval has elapsed minus `min(32 s, interval / 3)`,
/// giving a late refresh a chance to land first.
const SESSION_EXPIRY_HEADROOM_MAX: Duration = Duration::from_secs(32);

/// Methods this UAS accepts, advertised on `405 Method Not Allowed`
/// (RFC 3261 §8.2.1 requires the `Allow` header there).
const ALLOWED_METHODS: &str = "INVITE, ACK, CANCEL, BYE, OPTIONS, REGISTER, UPDATE";

/// RFC 4028 session-timer policy for the UAS side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionTimerConfig {
    /// Master switch. When `false` the UAS ignores `Session-Expires`
    /// entirely and never adds one to its answers.
    pub enabled: bool,
    /// Session interval offered when the peer supports timers but
    /// did not ask for a specific interval.
    pub default_expires: std::time::Duration,
    /// Smallest interval accepted; a request asking for less is
    /// refused with `422 Session Interval Too Small` + `Min-SE`.
    pub min_se: std::time::Duration,
}

impl Default for SessionTimerConfig {
    /// Enabled, 1800 s default interval, 90 s minimum (the RFC 4028
    /// §4 floor).
    fn default() -> Self {
        Self {
            enabled: true,
            default_expires: Duration::from_mins(30),
            min_se: Duration::from_secs(90),
        }
    }
}

/// First leg of a pending rendezvous bridge, waiting for a matching
/// second `INVITE`. Holds only tokens — the socket lives in the
/// [`MediaFabric`].
#[derive(Clone, Debug)]
struct PendingLeg {
    dialog_key: DialogKey,
    /// The leg as it will enter the bridge: endpoint, peer RTP /
    /// RTCP addresses, SRTP keys, clock rate.
    leg: BridgeLeg,
    /// Codec the negotiator chose for this leg, so the pairing step
    /// can detect a codec mismatch and route to the transcoded
    /// session path instead of the plain passthrough bridge.
    audio_codec: Option<NegotiatedCodec>,
}

/// Per-dialog CDR metadata captured at 200 OK INVITE. Consumed
/// on dialog termination to emit a `CallDetailRecord` via the
/// configured [`smiths_core::storage::CdrStore`].
#[derive(Clone, Debug)]
struct CdrInProgress {
    call_id: String,
    from_uri: String,
    to_uri: String,
    started_at_unix: i64,
}

/// Runtime view of one dialog's media leg — what a re-INVITE needs
/// to rebuild the bridge when the peer moves its RTP address or
/// changes keys. Not serialized: SRTP keys never go into snapshots.
#[derive(Clone, Debug, PartialEq)]
struct LegMedia {
    endpoint: EndpointId,
    remote: Option<SocketAddr>,
    rtcp_peer: Option<SocketAddr>,
    srtp: Option<SrtpKeys>,
    clock_rate: u32,
}

impl LegMedia {
    fn from_offer(endpoint: EndpointId, offer: &AcceptedOffer) -> Self {
        Self {
            endpoint,
            remote: offer.remote_media,
            rtcp_peer: offer.remote_rtcp,
            srtp: offer.srtp.clone(),
            clock_rate: offer.clock_rate,
        }
    }

    /// The leg as the media fabric wants it. `None` while the peer
    /// has not told us where it receives RTP.
    fn bridge_leg(&self) -> Option<BridgeLeg> {
        Some(BridgeLeg {
            endpoint: self.endpoint,
            peer: self.remote?,
            srtp: self.srtp.clone(),
            clock_rate: self.clock_rate,
            rtcp_peer: self.rtcp_peer,
        })
    }
}

/// Outcome of a successful offer/answer run.
struct AcceptedOffer {
    answer_body: String,
    remote_media: Option<SocketAddr>,
    /// Where the peer receives audio RTCP (`a=rtcp:` port, the RTP
    /// port under `a=rtcp-mux`, else RTP port + 1).
    remote_rtcp: Option<SocketAddr>,
    srtp: Option<SrtpKeys>,
    audio_codec: Option<NegotiatedCodec>,
    /// RTP clock rate of the audio codec (Hz).
    clock_rate: u32,
    video_codec: Option<NegotiatedCodec>,
    ice: Option<smiths_core::sdp::IceParams>,
}

/// Why an offer was refused; each maps to one SIP failure response.
enum OfferError {
    /// No common codec → `488`.
    Mismatch,
    /// Transport profile the engine can't terminate → `488` +
    /// `Warning: 399`.
    Unsupported(String),
    /// Unparseable SDP → `400`.
    Malformed(String),
}

/// Media negotiated for a new dialog: the allocated endpoint plus the
/// accepted offer.
struct NegotiatedMedia {
    endpoint: Arc<dyn smiths_core::media::MediaEndpoint>,
    offer: AcceptedOffer,
}

/// How the UAS attached a new dialog's media to the rest of the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaAttachment {
    /// No media, or a lone rendezvous leg parked awaiting its peer.
    None,
    /// Bridged with a pre-parked WebRTC leg, or parked there.
    WebRtc,
    /// Joined an N-party conference room.
    Conference,
    /// Paired with the waiting rendezvous leg.
    Bridged,
}

/// A final failure response the INVITE pipeline decided on.
struct Rejection {
    status: u16,
    reason: &'static str,
    warning: Option<String>,
}

/// RFC 4028 outcome for one INVITE / UPDATE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionTimerAgreement {
    /// Session interval in seconds.
    interval_secs: u32,
}

/// Seam the UAS uses to build a transcoded media session when a
/// rendezvous pair's two legs speak different codecs. Keeps
/// `smiths-sip` free of a direct `smiths-transcode` / `smiths-media`
/// dep — the CLI wires a concrete impl (typically
/// `smiths_media::TranscodedSession` + `smiths_transcode::CpuBudget`)
/// at boot.
///
/// `try_orchestrate` is consulted only when the two legs'
/// [`NegotiatedCodec`] values differ. Implementations return:
///
/// - `Ok(Some(session))` on admission success — the UAS installs
///   the session via [`DialogSessions`] and skips the plain
///   passthrough bridge.
/// - `Ok(None)` when admission is refused (CPU budget exhausted).
///   The UAS answers the second INVITE `503 Service Unavailable` with
///   `Warning: 370`, releases its media, and re-parks the first leg.
/// - `Err(_)` on fabric / codec construction failure; the UAS
///   answers `500 Server Internal Error` and re-parks the first leg.
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

/// Seam for installing a T.38 UDPTL session when both legs of a
/// bridged call re-INVITE into FAX. Implemented by
/// `smiths_fax::UdptlFaxOrchestrator`.
///
/// The UAS does not call this yet: wiring it needs T.38 offer/answer
/// in the [`SdpNegotiator`] plus an engine-originated re-INVITE
/// toward the far leg to learn its UDPTL address. Until then the
/// trait is the contract the fax crate builds against; there is no
/// `UasServer` setter for it.
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

/// Seam the UAS uses to place a dialog's media into an N-party
/// conference. The UAS reaches it through [`Self::orchestrate_room`]
/// for INVITEs whose Request-URI user-part carries the configured
/// conference-room prefix; the mixer implementation resolves the
/// room name to a conference id and delegates to
/// [`Self::try_orchestrate_conference`].
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
    /// `CSeq` sequence number.
    cseq: Option<u32>,
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
    /// Parsed by the REGISTER path to extract the contact URI and by
    /// the INVITE path for the dialog's remote target. `None` on
    /// requests that omit Contact entirely — common on OPTIONS + BYE
    /// where the header isn't mandatory.
    contact: Option<String>,
    /// `Record-Route` values in header order (one entry per URI).
    record_route: Vec<String>,
    /// URI-part of the `From:` header (`sip:bob@x`, no tag / params).
    from_uri: Option<String>,
    /// URI-part of the `To:` header (`sip:alice@y`, no tag / params).
    to_uri: Option<String>,
    /// Parsed `Expires:` header (RFC 3261 §20.19). For REGISTER the
    /// expiration is also carryable on each `Contact:` param via
    /// `;expires=N`; we honour the top-level header as the default
    /// and let the registrar override per-contact.
    expires: Option<u32>,
    /// RFC 4028 `Session-Expires` value in seconds.
    session_expires: Option<u32>,
    /// RFC 4028 `refresher=` parameter on `Session-Expires`
    /// (`uac` / `uas`), lowercased.
    session_refresher: Option<String>,
    /// RFC 4028 `Min-SE` value in seconds.
    min_se: Option<u32>,
    /// `Supported:` (or `k:`) lists `timer`.
    supports_timer: bool,
    /// Message body as UTF-8 (SDP is ASCII).
    body: Option<String>,
    /// Raw request bytes; the response builder copies header lines
    /// from them verbatim.
    raw: Bytes,
    /// `X-Smiths-Webrtc-Tag:`. Present only on INVITEs that want to
    /// join a pre-parked WebRTC leg sharing the same tag via the
    /// `[webrtc]` rendezvous map. Absent on every other request + on
    /// INVITEs from clients that don't care about WebRTC bridging.
    webrtc_tag: Option<String>,
}

impl RequestSummary {
    /// `true` when the body is an SDP offer.
    fn has_sdp(&self) -> bool {
        matches!(self.content_type.as_deref(), Some("application/sdp")) && self.body.is_some()
    }
}

/// Timer-driven event routed through the per-`Call-ID` worker so it
/// is ordered with the SIP messages of the same call.
#[derive(Debug)]
enum InternalEvent {
    /// The §13.3.1.4 2xx retransmit budget ran out without an ACK.
    AckTimeout { key: DialogKey },
    /// The RFC 4028 session interval elapsed with no refresh.
    /// `generation` identifies the arming; a refresh bumps it so a
    /// timer that fired concurrently with the refresh is ignored.
    SessionExpired { key: DialogKey, generation: u64 },
    /// The absolute maximum call duration elapsed.
    MaxDurationReached { key: DialogKey },
}

impl InternalEvent {
    fn key(&self) -> &DialogKey {
        match self {
            Self::AckTimeout { key }
            | Self::SessionExpired { key, .. }
            | Self::MaxDurationReached { key } => key,
        }
    }
}

/// Unit of work for a per-`Call-ID` worker.
enum WorkItem {
    Request(Box<RequestSummary>, SocketAddr),
    Internal(InternalEvent),
}

/// One armed session timer.
struct SessionTimer {
    cancel: CancellationToken,
    generation: u64,
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
    /// Runtime media view per dialog (endpoint, peer RTP address,
    /// SRTP keys) used to rebuild bridges on re-INVITE.
    leg_media: DashMap<DialogKey, LegMedia>,
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
    /// Non-passthrough media sessions (transcoded, conference) keyed
    /// per leg. Empty when only plain bridges are in use.
    dialog_sessions: DialogSessions,
    /// Optional transcoded-session builder. `None` = a rendezvous
    /// codec mismatch is refused with `488` (a passthrough bridge
    /// between different codecs would forward inaudible bytes). The
    /// CLI wires a real orchestrator from `[media.transcode]` config.
    transcode_orchestrator: Option<Arc<dyn TranscodeOrchestrator>>,
    /// Optional conference-participant session builder. `None` =
    /// conference rooms are not routed; every room uses the 2-peer
    /// rendezvous. CLI wires a real orchestrator from `[media.mixer]`
    /// config.
    conference_orchestrator: Option<Arc<dyn ConferenceOrchestrator>>,
    /// Request-URI user-part prefix that marks a *conference room*.
    /// When set and a conference orchestrator is wired, an INVITE
    /// whose room matches this prefix joins an N-party mixer (one
    /// participant per INVITE) instead of the 2-peer rendezvous.
    /// `None` = no conference routing.
    conference_room_prefix: Option<String>,
    /// Registrar: digest-auths `REGISTER` / `INVITE` against a
    /// [`crate::auth::CredentialStore`]. `None` = auth disabled,
    /// registrar accepts any REGISTER blindly (dev convenience; never
    /// do that in prod).
    registrar: Option<crate::auth::digest::Registrar>,
    /// Contact-binding persistence for registered UAs. `None` =
    /// in-memory REGISTER handling only (every successful REGISTER is
    /// 200 OK but the binding isn't persisted anywhere). Production
    /// deployments wire a `SqliteAuthStore` (or equivalent) here via
    /// [`Self::with_registration_store`] so `sip://registrations` has
    /// something to read.
    registration_store: Option<Arc<dyn crate::auth::RegistrationStore>>,
    /// Call-detail-record persistence. `None` = CDR recording is off.
    /// When wired, a row lands per dialog terminate with duration +
    /// From/To + result.
    cdr_store: Option<Arc<dyn smiths_core::storage::CdrStore>>,
    /// Per-dialog CDR metadata captured at 200 OK INVITE and consumed
    /// at termination. Kept off `DialogRecord` so the serializable
    /// snapshot surface (HA) stays clean — a failover primary that
    /// resumes mid-call won't emit a CDR for the old dialog it
    /// inherits, which is the correct posture.
    cdr_pending: Arc<DashMap<DialogKey, CdrInProgress>>,
    /// Prometheus metrics. Defaults to [`Metrics::noop`] so tests and
    /// single-server setups can ignore observability entirely.
    metrics: Arc<Metrics>,
    /// Shared correlator for responses to requests the
    /// [`crate::UacClient`] originated. `None` = UAS-only deployment;
    /// such responses are dropped.
    response_router: Option<Arc<crate::ResponseRouter>>,
    /// Shared graceful-drain flag. When set, new `INVITE`s are
    /// rejected with `503 Service Unavailable` so load balancers
    /// route traffic elsewhere while live dialogs finish naturally.
    /// `None` = drain disabled (tests, single-shot deployments).
    drain: Option<smiths_core::Drain>,
    /// Per-source-IP token bucket. Always present — when config
    /// disables rate limiting it's a cheap always-allow.
    rate_limit: crate::rate_limit::SipRateLimiter,
    /// Optional handle on the WebRTC rendezvous map. When present +
    /// an `INVITE` carries `X-Smiths-Webrtc-Tag:`, the UAS asks the
    /// rendezvous to bridge this SIP dialog with a WebRTC leg sharing
    /// the same tag instead of running the normal SIP-side rendezvous
    /// on the Request-URI user-part. `None` = the header is silently
    /// ignored (safe fallback for deployments without the WebRTC
    /// adapter wired).
    webrtc_rendezvous: Option<Arc<dyn smiths_core::WebRtcRendezvous>>,
    /// HA replicator. Standalone deployments use a no-op.
    replicator: Arc<dyn smiths_core::Replicator>,
    /// Async driver hosting every server-side transaction — both
    /// INVITE (`ServerInviteTxn` with G/H/I timers, ACK correlation,
    /// 2xx bypass) and non-INVITE (`ServerNonInviteTxn` with timer J)
    /// — plus the client transactions behind engine-originated
    /// in-dialog requests (BYE). INVITE 2xx bypasses the FSM and is
    /// TU-owned (see [`Self::invite_2xx_retransmits`]).
    txn_driver: TransactionDriver<T>,
    /// Router handed to [`Self::txn_driver`]. Responses to the UAS's
    /// own client transactions are matched on branch + `CSeq` method
    /// and fed straight into the driver, so nothing subscribes here;
    /// the driver constructor merely requires one.
    txn_router: Arc<crate::ResponseRouter>,
    /// RFC 4028 policy.
    session_timer: SessionTimerConfig,
    /// Absolute cap on a dialog's lifetime. `None` = unlimited.
    max_call_duration: Option<Duration>,
    /// How long the §13.3.1.4 2xx retransmit loop waits for an ACK
    /// before the dialog is ended: 64·T1 unless a test shortened it.
    invite_2xx_timeout: Duration,
    /// Armed session timers keyed by dialog.
    session_timers: DashMap<DialogKey, SessionTimer>,
    /// Armed max-call-duration guards keyed by dialog.
    duration_guards: DashMap<DialogKey, CancellationToken>,
    /// INVITE branches cancelled while the INVITE was still pending,
    /// mapped to the To-tag the `200 OK` to the CANCEL carried so the
    /// `487` can reuse it (RFC 3261 §9.2). The INVITE handler consumes
    /// the entry when it sends its final response.
    cancelled_invites: DashMap<String, String>,
    /// Live per-`Call-ID` workers. A worker retires when idle and
    /// removes its own entry.
    workers: DashMap<String, mpsc::UnboundedSender<WorkItem>>,
    /// Timer tasks push their expiry events here; [`Self::run`]
    /// forwards them to the owning `Call-ID` worker.
    internal_tx: mpsc::UnboundedSender<InternalEvent>,
    /// Receiving half of [`Self::internal_tx`], taken by [`Self::run`].
    internal_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<InternalEvent>>>,
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
        let txn_router = Arc::new(crate::ResponseRouter::new());
        let txn_driver = TransactionDriver::new(Arc::clone(&transport), Arc::clone(&txn_router));
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        Ok(Self {
            transport,
            bus,
            invite_2xx_retransmits: Arc::new(DashMap::new()),
            dialogs: Arc::new(DashMap::new()),
            leg_media: DashMap::new(),
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
            txn_router,
            session_timer: SessionTimerConfig::default(),
            max_call_duration: None,
            invite_2xx_timeout: TIMEOUT_64T1,
            session_timers: DashMap::new(),
            duration_guards: DashMap::new(),
            cancelled_invites: DashMap::new(),
            workers: DashMap::new(),
            internal_tx,
            internal_rx: std::sync::Mutex::new(Some(internal_rx)),
        })
    }

    /// Attach a [`smiths_core::WebRtcRendezvous`] handle so `INVITE`
    /// requests carrying `X-Smiths-Webrtc-Tag:` can bridge with a
    /// pre-parked WebRTC leg. `None` = the header is ignored.
    #[must_use]
    pub fn with_webrtc_rendezvous(
        mut self,
        rendezvous: Arc<dyn smiths_core::WebRtcRendezvous>,
    ) -> Self {
        self.webrtc_rendezvous = Some(rendezvous);
        self
    }

    /// Attach a digest registrar — `REGISTER` and `INVITE` now require
    /// valid auth.
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
        // The driver is cheap (just a fresh Arc-backed inner +
        // DashMap), so swapping it before any inbound traffic has
        // registered txns is fine.
        self.txn_driver =
            TransactionDriver::new(Arc::clone(&self.transport), Arc::clone(&self.txn_router))
                .with_metrics(Arc::clone(&metrics));
        self.metrics = metrics;
        self
    }

    /// Install a [`crate::ResponseRouter`] so responses arriving on
    /// the UAS's socket get forwarded to the UAC. Without this, the
    /// UAS drops responses to UAC-originated requests.
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
    /// different per-leg codecs route through a transcoded session.
    /// Without this, a codec mismatch at pairing time is refused with
    /// `488 Not Acceptable Here`.
    #[must_use]
    pub fn with_transcode_orchestrator(
        mut self,
        orchestrator: Arc<dyn TranscodeOrchestrator>,
    ) -> Self {
        self.transcode_orchestrator = Some(orchestrator);
        self
    }

    /// Attach a [`ConferenceOrchestrator`]. Takes effect together
    /// with [`Self::with_conference_rooms`].
    #[must_use]
    pub fn with_conference_orchestrator(
        mut self,
        orchestrator: Arc<dyn ConferenceOrchestrator>,
    ) -> Self {
        self.conference_orchestrator = Some(orchestrator);
        self
    }

    /// Mark a Request-URI user-part prefix as conference rooms. With
    /// a [`ConferenceOrchestrator`] also wired, INVITEs to
    /// `sip:<prefix>…@engine` join an N-party mixer instead of the
    /// 2-peer rendezvous. Without this, all rooms bridge as before.
    #[must_use]
    pub fn with_conference_rooms(mut self, prefix: impl Into<String>) -> Self {
        self.conference_room_prefix = Some(prefix.into());
        self
    }

    /// Inject an HA replicator.
    #[must_use]
    pub fn with_replicator(mut self, replicator: Arc<dyn smiths_core::Replicator>) -> Self {
        self.replicator = replicator;
        self
    }

    /// Inject an existing dialog table. Useful for sharing the table
    /// across multiple listeners in HA setups.
    #[must_use]
    pub fn with_dialogs(
        mut self,
        dialogs: Arc<dashmap::DashMap<smiths_core::DialogKey, smiths_core::DialogRecord>>,
    ) -> Self {
        self.dialogs = dialogs;
        self
    }

    /// Set the RFC 4028 session-timer policy. The default is
    /// [`SessionTimerConfig::default`] (enabled, 1800 s, 90 s).
    #[must_use]
    pub fn with_session_timer(mut self, cfg: SessionTimerConfig) -> Self {
        self.session_timer = cfg;
        self
    }

    /// Cap every dialog's lifetime: once `max` elapses after the 2xx
    /// the UAS sends BYE and releases the call's media. `None`
    /// (the default) = unlimited.
    #[must_use]
    pub fn with_max_call_duration(mut self, max: Option<std::time::Duration>) -> Self {
        self.max_call_duration = max;
        self
    }

    /// Override the RFC 3261 §13.3.1.4 ACK wait (64·T1 = 32 s) after
    /// which an unacknowledged 2xx ends the dialog. Exists so tests
    /// can exercise that path without waiting the real 32 s; nothing
    /// else should change it.
    #[doc(hidden)]
    #[must_use]
    pub fn with_invite_2xx_timeout(mut self, budget: Duration) -> Self {
        self.invite_2xx_timeout = budget;
        self
    }

    /// Snapshot handle on the runtime session table. Useful for tests
    /// asserting which transcoded / conference sessions have been
    /// installed.
    #[must_use]
    pub fn dialog_sessions(&self) -> &DialogSessions {
        &self.dialog_sessions
    }

    /// Shared handle on the dialog table. Cloned out so the CLI can
    /// take a live-dialog snapshot on graceful shutdown — the UAS's
    /// `run` consumes `self`, so without this accessor the snapshot
    /// path would need to live inside the UAS and duplicate the
    /// shutdown plumbing. The `Arc` + `DashMap` are cheap to share;
    /// concurrent read from the snapshot writer doesn't interfere
    /// with the live UAS modifying its own dialogs because
    /// `DashMap::iter` yields a consistent per-shard view.
    #[must_use]
    pub fn dialogs_handle(&self) -> Arc<DashMap<DialogKey, DialogRecord>> {
        Arc::clone(&self.dialogs)
    }

    /// Prime the dialog table with a set of pre-existing records
    /// (snapshot replay). Called by the CLI at boot before `run` if
    /// a snapshot file was loaded. Each restored dialog gets its
    /// record slot re-populated; the UAS then processes subsequent
    /// in-dialog requests (ACK, BYE, re-INVITE) exactly as if the
    /// record had been built by a live INVITE.
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

    // -----------------------------------------------------------------
    // Ingress: parse, transaction layer, dispatch to per-call workers
    // -----------------------------------------------------------------

    /// Run the UAS event loop. Exits when `cancel` fires or `rx` closes.
    ///
    /// The loop itself never blocks on Transaction-User work: every
    /// datagram is parsed, run through the transaction layer
    /// (retransmit replay, server-FSM registration, CANCEL matching)
    /// and then handed to the ordered worker for its `Call-ID`.
    /// Timer events from dialogs take the same worker path so they
    /// are serialized with the call's own SIP traffic.
    #[instrument(skip_all)]
    pub async fn run(self, mut rx: mpsc::Receiver<Datagram>, cancel: CancellationToken) {
        info!("UAS started");
        let this = Arc::new(self);
        let mut internal_rx = this
            .internal_rx
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
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
                    this.ingress(dg, &cancel).await;
                }
                ev = recv_internal(&mut internal_rx) => {
                    let call_id = ev.key().0.clone();
                    this.dispatch(&call_id, WorkItem::Internal(ev), &cancel);
                }
            }
        }
        info!("UAS stopped");
    }

    async fn ingress(self: &Arc<Self>, dg: Datagram, cancel: &CancellationToken) {
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
                self.admit_request(summary, peer, cancel).await;
            }
            rsip::SipMessage::Response(_) => self.route_response(dg.bytes, peer),
        }
    }

    /// Correlate an inbound response with a client transaction. RFC
    /// 3261 §17.1.3 matches on the top `Via` branch **and** the `CSeq`
    /// method: the UAS's own client FSMs (engine-originated BYE) are
    /// keyed that way in the driver. Anything else goes to the
    /// [`crate::UacClient`]'s router, which keys on the branch alone.
    fn route_response(&self, bytes: Bytes, peer: SocketAddr) {
        let Some(branch) = extract_via_branch(&bytes) else {
            debug!(%peer, "response without Via branch; dropped");
            return;
        };
        if let Some(method) = crate::txn::cseq_method(&bytes) {
            let key = TxnKey {
                branch: branch.clone(),
                method,
                role: TxnRole::Client,
            };
            if self.txn_driver.is_alive(&key) {
                let status = response_status(&bytes).unwrap_or(0);
                self.txn_driver.deliver_response(&key, status, bytes);
                return;
            }
        }
        if let Some(router) = self.response_router.as_ref() {
            if !router.deliver(&branch, bytes) {
                debug!(%peer, branch, "response with unknown branch; dropped");
            }
        } else {
            debug!(%peer, "ignoring response (no UAC attached)");
        }
    }

    /// Transaction-layer admission for one request, then dispatch to
    /// the owning `Call-ID` worker.
    ///
    /// Every method the UAS responds to — INVITE / OPTIONS / BYE /
    /// REGISTER / CANCEL / UPDATE / unknown-405 — lives as a
    /// `ServerInviteTxn` or `ServerNonInviteTxn` in the driver.
    /// Retransmits feed into `deliver_request` so the FSM replays its
    /// cached final response; new requests register a fresh FSM entry
    /// before the handler runs, so the subsequent [`Self::respond`]
    /// call routes through `send_response`.
    ///
    /// ACK is the exception on both sides: it never gets its own
    /// transaction (RFC 3261 §17.1.1.3 makes ACK for non-2xx part
    /// of the INVITE transaction; ACK for 2xx is end-to-end per
    /// §13.3.1.4). [`Self::handle_ack`] reaches into the INVITE
    /// FSM directly for the non-2xx → Confirmed transition.
    async fn admit_request(
        self: &Arc<Self>,
        req: RequestSummary,
        peer: SocketAddr,
        cancel: &CancellationToken,
    ) {
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

        if let Some(branch) = req.branch.as_deref()
            && req.method != "ACK"
        {
            let key = server_txn_key(branch, &req.method);
            if self.txn_driver.is_alive(&key) {
                // Retransmit: FSM replays its cached response.
                debug!(%peer, branch, method = %req.method, "retransmit → server FSM");
                self.txn_driver
                    .deliver_request(&key, req.method.clone(), req.raw.clone());
                return;
            }
            // An INVITE retransmit arriving after the FSM has
            // 2xx-bypassed to Terminated is dropped — the per-dialog
            // retransmit loop owns replay cadence (RFC 3261
            // §13.3.1.4). Answering a peer retry here would inject an
            // off-schedule 2xx and break the T1-doubling contract.
            if req.method == "INVITE" && self.is_invite_retransmit(&req) {
                debug!(
                    %peer, branch,
                    "INVITE retransmit for dialog with live 2xx loop; dropped (TU drives replay)"
                );
                return;
            }
            // Fresh transaction — register before the handler runs
            // so `respond` / `send_provisional` route through
            // `driver.send_response`.
            let txn: Box<dyn crate::txn::Transaction> = if req.method == "INVITE" {
                Box::new(ServerInviteTxn::new(branch.to_string()))
            } else {
                Box::new(ServerNonInviteTxn::new(
                    branch.to_string(),
                    req.method.clone(),
                ))
            };
            let _tu_rx = match crate::txn::top_via_sent_by(&req.raw) {
                Some(sent_by) => self
                    .txn_driver
                    .start_server_with_sent_by(txn, peer, sent_by),
                None => self.txn_driver.start_server(txn, peer),
            };
            if req.method == "CANCEL" && self.cancel_pending_invite(&req, peer).await {
                return;
            }
        }

        let call_id = req.call_id.clone().unwrap_or_default();
        self.dispatch(&call_id, WorkItem::Request(Box::new(req), peer), cancel);
    }

    /// `true` when `req` re-sends the INVITE transaction a live dialog
    /// was created (or last re-negotiated) by: same `Call-ID`, same
    /// tags, same `Via` branch (RFC 3261 §17.2.3). The server FSM is
    /// already gone after a 2xx, so this is the only way to tell such
    /// a retransmit from a genuinely new INVITE on the same call.
    fn is_invite_retransmit(&self, req: &RequestSummary) -> bool {
        let (Some(branch), Some(call_id), Some(remote_tag)) = (
            req.branch.as_deref(),
            req.call_id.as_deref(),
            req.from_tag.as_deref(),
        ) else {
            return false;
        };
        if let Some(local_tag) = req.to_tag.as_deref() {
            let key: DialogKey = (
                call_id.to_owned(),
                local_tag.to_owned(),
                remote_tag.to_owned(),
            );
            return self
                .dialogs
                .get(&key)
                .is_some_and(|r| r.last_invite_branch.as_deref() == Some(branch));
        }
        // No To-tag: the original INVITE. Any dialog it created (there
        // is normally one) remembers its branch.
        self.dialogs.iter().any(|e| {
            let k = e.key();
            k.0 == call_id && k.2 == remote_tag && e.last_invite_branch.as_deref() == Some(branch)
        })
    }

    /// RFC 3261 §9.2 for a CANCEL whose INVITE server transaction is
    /// still alive. Returns `true` when the CANCEL was fully answered
    /// here; `false` hands it to the `Call-ID` worker, which knows
    /// whether a dialog answered that INVITE with a 2xx.
    ///
    /// - INVITE still in `Proceeding` (no final response yet): the
    ///   INVITE is flagged as cancelled and the CANCEL gets `200 OK`
    ///   at once. The INVITE handler — possibly mid-flight on its
    ///   worker — sees the flag before it would send its final and
    ///   answers `487 Request Terminated` with the same To-tag.
    /// - INVITE already has a non-2xx final: `200 OK`, no effect.
    async fn cancel_pending_invite(&self, req: &RequestSummary, peer: SocketAddr) -> bool {
        let Some(branch) = req.branch.as_deref() else {
            return false;
        };
        let invite_key = server_txn_key(branch, "INVITE");
        match self.txn_driver.state(&invite_key) {
            Some(TransactionState::Proceeding) => {
                let tag = next_tag();
                self.cancelled_invites
                    .insert(branch.to_owned(), tag.clone());
                info!(%peer, branch, "CANCEL matched pending INVITE");
                self.respond(req, 200, "OK", Some(&tag), &[], b"", peer)
                    .await;
                true
            }
            Some(_) => {
                debug!(%peer, branch, "CANCEL after INVITE final response; no effect");
                self.respond(req, 200, "OK", Some(&next_tag()), &[], b"", peer)
                    .await;
                true
            }
            None => false,
        }
    }

    /// Hand `item` to the worker owning `call_id`, spawning one if
    /// none is live. Workers retire themselves when idle (see
    /// [`Self::run_worker`]); a send that fails because a worker
    /// retired between our lookup and the send simply respawns.
    fn dispatch(self: &Arc<Self>, call_id: &str, item: WorkItem, cancel: &CancellationToken) {
        let mut item = item;
        loop {
            if let Some(tx) = self.workers.get(call_id) {
                match tx.send(item) {
                    Ok(()) => return,
                    Err(mpsc::error::SendError(returned)) => item = returned,
                }
            }
            let (tx, rx) = mpsc::unbounded_channel();
            self.workers.insert(call_id.to_owned(), tx);
            tokio::spawn(Self::run_worker(
                Arc::clone(self),
                call_id.to_owned(),
                rx,
                cancel.clone(),
            ));
        }
    }

    /// Ordered worker for one `Call-ID`. Drains its queue in FIFO
    /// order and retires after [`WORKER_IDLE`] without traffic. The
    /// retirement check holds the workers-map entry lock while it
    /// peeks the queue, so an ingress send racing the retirement
    /// either lands before the check (and is processed) or finds no
    /// entry (and spawns a successor) — never a message in a queue
    /// nobody drains.
    async fn run_worker(
        this: Arc<Self>,
        call_id: String,
        mut rx: mpsc::UnboundedReceiver<WorkItem>,
        cancel: CancellationToken,
    ) {
        loop {
            let item = tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                res = tokio::time::timeout(WORKER_IDLE, rx.recv()) => match res {
                    Ok(Some(item)) => item,
                    Ok(None) => break,
                    Err(_idle) => {
                        let entry = this.workers.entry(call_id.clone());
                        if let Ok(item) = rx.try_recv() {
                            drop(entry);
                            item
                        } else {
                            if let dashmap::mapref::entry::Entry::Occupied(e) = entry {
                                e.remove();
                            }
                            break;
                        }
                    }
                },
            };
            match item {
                WorkItem::Request(req, peer) => this.handle_request(&req, peer).await,
                WorkItem::Internal(ev) => this.handle_internal(ev).await,
            }
        }
    }

    // -----------------------------------------------------------------
    // Transaction-User dispatch (runs on the Call-ID worker)
    // -----------------------------------------------------------------

    async fn handle_request(&self, req: &RequestSummary, peer: SocketAddr) {
        match req.method.as_str() {
            "OPTIONS" => self.handle_options(req, peer).await,
            "INVITE" if req.to_tag.is_some() => self.handle_reinvite(req, peer).await,
            "INVITE" => self.handle_invite(req, peer).await,
            "ACK" => self.handle_ack(req, peer),
            "BYE" => self.handle_bye(req, peer).await,
            "CANCEL" => self.handle_cancel(req, peer).await,
            "UPDATE" => self.handle_update(req, peer).await,
            "REGISTER" => self.handle_register(req, peer).await,
            _ => {
                self.respond(
                    req,
                    405,
                    "Method Not Allowed",
                    Some(&next_tag()),
                    &[("Allow", ALLOWED_METHODS)],
                    b"",
                    peer,
                )
                .await;
            }
        }
    }

    async fn handle_internal(&self, ev: InternalEvent) {
        match ev {
            InternalEvent::AckTimeout { key } => {
                let still_early = self
                    .dialogs
                    .get(&key)
                    .is_some_and(|r| r.state == DialogState::Early);
                if still_early {
                    warn!(?key, "no ACK within 64·T1; terminating dialog with BYE");
                    self.end_call(&key, DialogEvent::Error, "ack_timeout").await;
                }
            }
            InternalEvent::SessionExpired { key, generation } => {
                let current = self
                    .session_timers
                    .get(&key)
                    .is_some_and(|t| t.generation == generation);
                if current {
                    info!(
                        ?key,
                        "RFC 4028 session interval elapsed without refresh; sending BYE"
                    );
                    self.end_call(&key, DialogEvent::ByeCompleted, "session_expired")
                        .await;
                }
            }
            InternalEvent::MaxDurationReached { key } => {
                if self.dialogs.contains_key(&key) {
                    info!(?key, "maximum call duration reached; sending BYE");
                    self.end_call(&key, DialogEvent::ByeCompleted, "max_duration")
                        .await;
                }
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
        match self.digest_authenticate(reg, req, "REGISTER").await {
            AuthOutcome::Authenticated(user) => {
                info!(%user, %peer, "REGISTER authenticated");
                self.persist_register_binding(req, reg.realm(), &user);
                self.respond(req, 200, "OK", Some(&next_tag()), &[], b"", peer)
                    .await;
            }
            AuthOutcome::Challenge(challenge) => {
                let hdr: [(&str, &str); 1] = [("WWW-Authenticate", &challenge)];
                self.respond(req, 401, "Unauthorized", Some(&next_tag()), &hdr, b"", peer)
                    .await;
            }
            AuthOutcome::Failed => {
                self.respond(
                    req,
                    500,
                    "Server Internal Error",
                    Some(&next_tag()),
                    &[],
                    b"",
                    peer,
                )
                .await;
            }
        }
    }

    /// Persist the `Contact:` → expiry binding a successful REGISTER
    /// just established. Silent no-op when no
    /// [`crate::auth::RegistrationStore`] is attached; the auth flow
    /// has already decided the request is legitimate by this point.
    ///
    /// The parse is permissive — anything inside the first `<...>` on
    /// the Contact line is the URI; otherwise we take the first
    /// whitespace-delimited token. RFC 3261 §25.1 multi-contact +
    /// `expires=` parameters are not parsed; they matter for forking
    /// proxies more than for a registrar that binds one AOR at a time.
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

    /// Run digest authentication for `req`. The credential lookup
    /// may block (`SQLite`, or the HTTP store's `block_in_place` +
    /// `block_on`), so it runs on the blocking pool: this worker
    /// waits, every other `Call-ID` keeps flowing.
    async fn digest_authenticate(
        &self,
        reg: &crate::auth::digest::Registrar,
        req: &RequestSummary,
        method: &str,
    ) -> AuthOutcome {
        use crate::auth::digest::{Algorithm, AuthError};
        let Some(auth) = req.authorization.clone() else {
            return AuthOutcome::Challenge(reg.challenge(Algorithm::Md5, false));
        };
        let ruri = req.request_uri.clone().unwrap_or_default();
        let method = method.to_owned();
        let registrar = reg.clone();
        let verdict =
            tokio::task::spawn_blocking(move || registrar.authenticate(&method, &ruri, &auth))
                .await;
        match verdict {
            Ok(Ok(user)) => AuthOutcome::Authenticated(user),
            Ok(Err(e)) => {
                info!(?e, "digest auth failed; re-challenging");
                let stale = matches!(e, AuthError::StaleNonce | AuthError::NonceReplayed);
                AuthOutcome::Challenge(reg.challenge(Algorithm::Md5, stale))
            }
            Err(join_err) => {
                warn!(?join_err, "credential lookup task failed");
                AuthOutcome::Failed
            }
        }
    }

    // -----------------------------------------------------------------
    // INVITE — dialog creation
    // -----------------------------------------------------------------

    /// Dialog-creating INVITE: admission (drain, auth, provisional),
    /// RFC 4028 negotiation, offer/answer, media attachment
    /// (WebRTC / conference / rendezvous), then the 2xx. A CANCEL that
    /// raced any of the awaits is honoured at the checkpoints before
    /// media is committed.
    #[instrument(skip_all, fields(%peer, call_id = %req.call_id.as_deref().unwrap_or("-")))]
    async fn handle_invite(&self, req: &RequestSummary, peer: SocketAddr) {
        if self.invite_was_cancelled(req) {
            self.reject_cancelled_invite(req, peer).await;
            return;
        }
        let Some((call_id, remote_tag)) = self.admit_invite(req, peer).await else {
            return;
        };
        let timer = match self.negotiate_session_timer(req) {
            Ok(t) => t,
            Err(min_se) => {
                self.respond_interval_too_small(req, peer, min_se).await;
                return;
            }
        };
        let Ok(media) = self.negotiate_initial_offer(req, peer).await else {
            return;
        };
        if self.invite_was_cancelled(req) {
            self.release_negotiated(media.as_ref()).await;
            self.reject_cancelled_invite(req, peer).await;
            return;
        }
        let local_tag = self.take_invite_tag(req);
        let dialog_key: DialogKey = (call_id, local_tag, remote_tag);
        let attachment = match self.attach_media(req, &dialog_key, media.as_ref()).await {
            Ok(a) => a,
            Err(rejection) => {
                self.release_negotiated(media.as_ref()).await;
                self.reject_invite(req, peer, &dialog_key.1, &rejection)
                    .await;
                return;
            }
        };
        self.establish_dialog(req, peer, dialog_key, media, timer, attachment)
            .await;
    }

    /// Drain check, digest auth, `100 Trying`, and the mandatory
    /// dialog identifiers. Returns `(Call-ID, From-tag)` when the
    /// INVITE may proceed; every refusal has already been answered.
    async fn admit_invite(
        &self,
        req: &RequestSummary,
        peer: SocketAddr,
    ) -> Option<(String, String)> {
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
                Some(&self.take_invite_tag(req)),
                &[("Retry-After", "0")],
                &[],
                peer,
            )
            .await;
            return None;
        }

        // When a registrar is attached, INVITE requires digest auth. We
        // challenge before emitting 100 Trying so the rejection path
        // stays tight — no media allocation, no dialog state, just the
        // 401 back to the caller. The ACK that closes the rejected
        // transaction is handled by the normal ACK dispatch.
        if !self.invite_auth_ok(req, peer).await {
            return None;
        }

        // 100 Trying short-circuits UDP INVITE retransmission.
        self.send_provisional(req, 100, "Trying", peer).await;

        let call_id = req.call_id.clone().unwrap_or_default();
        let remote_tag = req.from_tag.clone().unwrap_or_default();
        if call_id.is_empty() || remote_tag.is_empty() {
            warn!(%peer, "INVITE missing Call-ID or From-tag; rejecting 400");
            self.respond(
                req,
                400,
                "Bad Request",
                Some(&self.take_invite_tag(req)),
                &[],
                &[],
                peer,
            )
            .await;
            return None;
        }
        Some((call_id, remote_tag))
    }

    /// Digest-authenticate an incoming INVITE. Returns `true` when the
    /// request may proceed; emits the appropriate `401 Unauthorized`
    /// (or `500` when the credential store itself failed) and returns
    /// `false` otherwise. No registrar attached → every INVITE is
    /// waved through (dev mode, matching `handle_register`).
    async fn invite_auth_ok(&self, req: &RequestSummary, peer: SocketAddr) -> bool {
        let Some(reg) = self.registrar.as_ref() else {
            return true;
        };
        match self.digest_authenticate(reg, req, "INVITE").await {
            AuthOutcome::Authenticated(user) => {
                info!(%user, %peer, "INVITE authenticated");
                true
            }
            AuthOutcome::Challenge(challenge) => {
                let hdr: [(&str, &str); 1] = [("WWW-Authenticate", &challenge)];
                self.respond(
                    req,
                    401,
                    "Unauthorized",
                    Some(&self.take_invite_tag(req)),
                    &hdr,
                    b"",
                    peer,
                )
                .await;
                false
            }
            AuthOutcome::Failed => {
                self.respond(
                    req,
                    500,
                    "Server Internal Error",
                    Some(&self.take_invite_tag(req)),
                    &[],
                    b"",
                    peer,
                )
                .await;
                false
            }
        }
    }

    /// Allocate a media endpoint and run offer/answer for a
    /// dialog-creating INVITE. `Ok(None)` when the INVITE carried no
    /// SDP (a media-less dialog). Every failure has been answered
    /// and the endpoint released before `Err` comes back.
    async fn negotiate_initial_offer(
        &self,
        req: &RequestSummary,
        peer: SocketAddr,
    ) -> Result<Option<NegotiatedMedia>, ()> {
        if !req.has_sdp() {
            return Ok(None);
        }
        let endpoint = match self.media_fabric.allocate(self.media_bind_ip).await {
            Ok(ep) => ep,
            Err(e) => {
                warn!(?e, "failed to allocate media endpoint for INVITE");
                self.respond(
                    req,
                    500,
                    "Server Internal Error",
                    Some(&self.take_invite_tag(req)),
                    &[],
                    &[],
                    peer,
                )
                .await;
                return Err(());
            }
        };
        let body = req.body.as_deref().unwrap_or_default();
        let local_ip = self.effective_local_ip(peer).await;
        match self.negotiate_body(body, local_ip, endpoint.local_addr().port()) {
            Ok(offer) => Ok(Some(NegotiatedMedia { endpoint, offer })),
            Err(e) => {
                self.media_fabric.release_endpoint(endpoint.id()).await;
                let tag = self.take_invite_tag(req);
                self.reject_offer(req, peer, &tag, e).await;
                Err(())
            }
        }
    }

    /// Run the negotiator against `body`, publishing `local_ip:port`
    /// as the engine's media address. Video is declined (`video_port
    /// = None`): the negotiator preserves m-line ordering by emitting
    /// an RFC 3264 port-0 answer for any `m=video` in the offer.
    fn negotiate_body(
        &self,
        body: &str,
        local_ip: IpAddr,
        local_port: u16,
    ) -> Result<AcceptedOffer, OfferError> {
        match self.negotiator.negotiate(body, local_ip, local_port, None) {
            NegotiationOutcome::Accepted {
                answer_body,
                remote_media,
                remote_rtcp_port,
                // `remote_rtcp_port` already equals the RTP port when
                // the peer negotiated rtcp-mux.
                rtcp_mux: _rtcp_mux,
                // Video passthrough is declined on the answer, so the
                // peer's video address is not used.
                video_media: _video_media,
                srtp,
                // DTLS-SRTP parameters are consumed by the
                // WebRTC-native adapter; the SIP UAS does not drive a
                // DTLS handshake.
                dtls: _dtls,
                audio_codec,
                audio_clock_rate,
                video_codec,
                ice,
            } => Ok(AcceptedOffer {
                answer_body,
                remote_media,
                remote_rtcp: remote_media
                    .zip(remote_rtcp_port)
                    .map(|(rtp, port)| SocketAddr::new(rtp.ip(), port)),
                srtp,
                audio_codec,
                clock_rate: audio_clock_rate.unwrap_or(smiths_core::media::DEFAULT_RTP_CLOCK_RATE),
                video_codec,
                ice,
            }),
            NegotiationOutcome::Mismatch => Err(OfferError::Mismatch),
            NegotiationOutcome::UnsupportedTransport { reason } => {
                Err(OfferError::Unsupported(reason))
            }
            NegotiationOutcome::Malformed(err) => Err(OfferError::Malformed(err)),
        }
    }

    /// Answer a refused offer: `488` for no common codec, `488` +
    /// `Warning: 399` for an unsupported transport profile, `400` for
    /// unparseable SDP.
    async fn reject_offer(
        &self,
        req: &RequestSummary,
        peer: SocketAddr,
        tag: &str,
        err: OfferError,
    ) {
        match err {
            OfferError::Mismatch => {
                info!(%peer, "SDP offer had no acceptable codec; 488");
                self.respond(req, 488, "Not Acceptable Here", Some(tag), &[], &[], peer)
                    .await;
            }
            OfferError::Unsupported(reason) => {
                info!(%peer, %reason, "SDP offer used an unsupported transport; 488 + Warning");
                let warning = format_warning(399, &reason);
                let warning_hdr: [(&str, &str); 1] = [("Warning", warning.as_str())];
                self.respond(
                    req,
                    488,
                    "Not Acceptable Here",
                    Some(tag),
                    &warning_hdr,
                    &[],
                    peer,
                )
                .await;
            }
            OfferError::Malformed(err) => {
                warn!(%peer, %err, "malformed SDP offer");
                self.respond(req, 400, "Bad Request", Some(tag), &[], &[], peer)
                    .await;
            }
        }
    }

    /// IP to publish in SDP answers and engine-originated headers for
    /// `peer`: the configured advertise address, else the bind IP, else
    /// (wildcard bind) whatever the kernel would route to `peer` with.
    async fn effective_local_ip(&self, peer: SocketAddr) -> IpAddr {
        match self.sdp_advertise_ip {
            Some(ip) => ip,
            None => crate::transport::resolve_local_ip_for(self.media_bind_ip, peer).await,
        }
    }

    async fn release_negotiated(&self, media: Option<&NegotiatedMedia>) {
        if let Some(m) = media {
            self.media_fabric.release_endpoint(m.endpoint.id()).await;
        }
    }

    /// Connect a new dialog's media to the rest of the call: a
    /// pre-parked WebRTC leg when the INVITE names one, a conference
    /// room when the Request-URI matches the configured prefix, else
    /// the 2-peer rendezvous keyed by Request-URI user-part. `Err`
    /// carries the final response the INVITE must get instead of a
    /// 2xx.
    async fn attach_media(
        &self,
        req: &RequestSummary,
        dialog_key: &DialogKey,
        media: Option<&NegotiatedMedia>,
    ) -> Result<MediaAttachment, Rejection> {
        let Some(m) = media else {
            return Ok(MediaAttachment::None);
        };
        let leg_media = LegMedia::from_offer(m.endpoint.id(), &m.offer);
        let Some(leg) = leg_media.bridge_leg() else {
            return Ok(MediaAttachment::None);
        };
        let endpoint = leg.endpoint;
        let remote_rtp = leg.peer;
        let srtp = leg.srtp.clone();

        // A matching `X-Smiths-Webrtc-Tag:` joins the shared
        // pending-legs map instead of the local Request-URI one. A
        // match installs the bridge through the WebRTC handler; a
        // miss parks the SIP leg there until its WebRTC partner
        // arrives. Header without rendezvous wired = silent ignore.
        if let (Some(tag), Some(rdv)) = (req.webrtc_tag.as_deref(), self.webrtc_rendezvous.as_ref())
        {
            match rdv
                .pair_sip_leg(tag, endpoint, remote_rtp, srtp.clone())
                .await
            {
                Ok(Some(bid)) => {
                    self.bridges_by_dialog.insert(dialog_key.clone(), bid);
                    info!(%tag, ?bid, "webrtc rendezvous: SIP dialog bridged to WebRTC partner");
                    return Ok(MediaAttachment::WebRtc);
                }
                Ok(None) => {
                    info!(%tag, "webrtc rendezvous: SIP leg parked awaiting WebRTC partner");
                    return Ok(MediaAttachment::WebRtc);
                }
                Err(e) => {
                    warn!(%tag, ?e, "webrtc rendezvous: pair_sip_leg failed; falling through");
                }
            }
        }

        let Some(room) = req.ruri_user.as_deref() else {
            return Ok(MediaAttachment::None);
        };

        // Conference rooms: an INVITE whose room matches the
        // configured prefix joins an N-party mixer right away — one
        // participant per INVITE, no pairing or parking. The session
        // is filed under `dialog_sessions` so teardown stops it
        // alongside transcoded sessions. A decline / error falls
        // through to the classic 2-peer rendezvous.
        if let (Some(orch), Some(prefix)) = (
            self.conference_orchestrator.as_ref(),
            self.conference_room_prefix.as_deref(),
        ) && room.starts_with(prefix)
        {
            match orch
                .orchestrate_room(dialog_key.clone(), room, leg.clone())
                .await
            {
                Ok(Some(session)) => {
                    use smiths_core::{LegId, MediaKindTag};
                    self.dialog_sessions.install(
                        dialog_key.clone(),
                        (LegId(0), MediaKindTag::Audio),
                        session,
                    );
                    info!(%room, "conference participant joined");
                    return Ok(MediaAttachment::Conference);
                }
                Ok(None) => warn!(%room, "conference declined; falling back to rendezvous"),
                Err(e) => warn!(%room, ?e, "conference join failed; falling back to rendezvous"),
            }
        }

        self.rendezvous_pair(room, dialog_key, leg, m.offer.audio_codec.clone())
            .await
    }

    /// Pair `leg` with the rendezvous leg already parked under `room`,
    /// or park it. The map entry is taken under its lock so two
    /// callers racing for the same room on different workers cannot
    /// both park and orphan each other.
    async fn rendezvous_pair(
        &self,
        room: &str,
        dialog_key: &DialogKey,
        leg: BridgeLeg,
        audio_codec: Option<NegotiatedCodec>,
    ) -> Result<MediaAttachment, Rejection> {
        use dashmap::mapref::entry::Entry;
        let pending = match self.pending_bridges.entry(room.to_owned()) {
            Entry::Occupied(e) => e.remove(),
            Entry::Vacant(e) => {
                e.insert(PendingLeg {
                    dialog_key: dialog_key.clone(),
                    leg,
                    audio_codec,
                });
                info!(rendezvous = %room, "rendezvous leg parked, awaiting peer");
                return Ok(MediaAttachment::None);
            }
        };
        let leg_a = pending.leg.clone();
        let outcome = match (pending.audio_codec.clone(), audio_codec) {
            (Some(codec_a), Some(codec_b)) if codec_a != codec_b => {
                self.try_orchestrate_transcoded(
                    room,
                    &pending.dialog_key,
                    dialog_key,
                    leg_a,
                    codec_a,
                    leg,
                    codec_b,
                )
                .await
            }
            _ => match self.media_fabric.bridge(leg_a, leg).await {
                Ok(bid) => {
                    self.bridges_by_dialog
                        .insert(pending.dialog_key.clone(), bid);
                    self.bridges_by_dialog.insert(dialog_key.clone(), bid);
                    info!(rendezvous = %room, "rendezvous bridge established");
                    Ok(())
                }
                Err(e) => {
                    warn!(?e, rendezvous = %room, "rendezvous bridge failed");
                    Err(Rejection {
                        status: 500,
                        reason: "Server Internal Error",
                        warning: None,
                    })
                }
            },
        };
        match outcome {
            Ok(()) => Ok(MediaAttachment::Bridged),
            Err(rejection) => {
                // Leg A is still a perfectly good call waiting for a
                // partner; put it back for the next caller.
                self.pending_bridges.insert(room.to_owned(), pending);
                Err(rejection)
            }
        }
    }

    /// Route a codec-mismatched rendezvous pair through a transcoded
    /// session. `Ok` means the orchestrator admitted the call and the
    /// session is installed under both legs' keys in
    /// [`DialogSessions`]. `Err` carries the response the second
    /// INVITE gets instead: `488` when no orchestrator is wired (a
    /// passthrough bridge between different codecs would forward
    /// inaudible bytes), `503` + `Warning: 370` when admission was
    /// refused, `500` when construction failed.
    #[allow(clippy::too_many_arguments)] // two legs × (key, leg, codec) plus the room
    async fn try_orchestrate_transcoded(
        &self,
        rendezvous_key: &str,
        dialog_a: &DialogKey,
        dialog_b: &DialogKey,
        leg_a: BridgeLeg,
        codec_a: NegotiatedCodec,
        leg_b: BridgeLeg,
        codec_b: NegotiatedCodec,
    ) -> Result<(), Rejection> {
        let Some(orch) = self.transcode_orchestrator.as_ref() else {
            warn!(
                rendezvous = rendezvous_key,
                %codec_a,
                %codec_b,
                "codec mismatch at rendezvous and no TranscodeOrchestrator wired; 488",
            );
            return Err(Rejection {
                status: 488,
                reason: "Not Acceptable Here",
                warning: Some(format_warning(
                    399,
                    &format!(
                        "codec mismatch ({codec_a} vs {codec_b}) and no transcoder configured"
                    ),
                )),
            });
        };
        match orch
            .try_orchestrate(leg_a, codec_a.clone(), leg_b, codec_b.clone())
            .await
        {
            Ok(Some(session)) => {
                // Install the same session handle under both legs'
                // keys: `(LegId(0), Audio)` for the first-in leg,
                // `(LegId(1), Audio)` for the second — the
                // `per_leg_codec` convention.
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
                Ok(())
            }
            Ok(None) => {
                warn!(
                    rendezvous = rendezvous_key,
                    %codec_a,
                    %codec_b,
                    "transcode admission refused (budget exhausted); 503",
                );
                Err(Rejection {
                    status: 503,
                    reason: "Service Unavailable",
                    warning: Some(format_warning(370, "transcoding capacity exhausted")),
                })
            }
            Err(e) => {
                warn!(
                    rendezvous = rendezvous_key,
                    %codec_a,
                    %codec_b,
                    error = %e,
                    "transcoded session construction failed; 500",
                );
                Err(Rejection {
                    status: 500,
                    reason: "Server Internal Error",
                    warning: None,
                })
            }
        }
    }

    /// Send the final failure `rejection` decided by the media
    /// attachment step.
    async fn reject_invite(
        &self,
        req: &RequestSummary,
        peer: SocketAddr,
        tag: &str,
        rejection: &Rejection,
    ) {
        let mut extras: Vec<(&str, &str)> = Vec::new();
        if let Some(w) = rejection.warning.as_deref() {
            extras.push(("Warning", w));
        }
        if rejection.status == 503 {
            extras.push(("Retry-After", "5"));
        }
        self.respond(
            req,
            rejection.status,
            rejection.reason,
            Some(tag),
            &extras,
            b"",
            peer,
        )
        .await;
    }

    /// Record the dialog, answer `200 OK`, and arm the dialog's
    /// timers.
    async fn establish_dialog(
        &self,
        req: &RequestSummary,
        peer: SocketAddr,
        dialog_key: DialogKey,
        media: Option<NegotiatedMedia>,
        timer: Option<SessionTimerAgreement>,
        attachment: MediaAttachment,
    ) {
        let (call_id, local_tag, remote_tag) = dialog_key.clone();
        // `LegId(0)` is the answerer's own leg — the leg whose codec
        // the negotiator just chose. A video codec lives under
        // `LegId(1)` so it cannot collide with the audio entry.
        let mut per_leg_codec = std::collections::BTreeMap::new();
        if let Some(m) = &media {
            if let Some(c) = m.offer.audio_codec.clone() {
                per_leg_codec.insert(smiths_core::LegId(0), c);
            }
            if let Some(c) = m.offer.video_codec.clone() {
                per_leg_codec.insert(smiths_core::LegId(1), c);
            }
        }
        let record = DialogRecord {
            call_id: call_id.clone(),
            local_tag: local_tag.clone(),
            remote_tag,
            state: DialogState::Early,
            peer_signal: peer,
            rendezvous: req.ruri_user.clone(),
            media: media.as_ref().map(|m| m.endpoint.id()),
            remote_media: media.as_ref().and_then(|m| m.offer.remote_media),
            pending_2xx: None,
            per_leg_codec,
            ice: media.as_ref().and_then(|m| m.offer.ice.clone()),
            remote_target: req.contact.as_deref().and_then(first_contact_uri),
            route_set: req.record_route.clone(),
            local_uri: req.to_uri.clone(),
            remote_uri: req.from_uri.clone(),
            local_cseq: 0,
            remote_cseq: req.cseq,
            transport: Some(self.transport.kind().via_token().to_owned()),
            last_invite_branch: req.branch.clone(),
            local_media: media.as_ref().map(|m| m.endpoint.local_addr()),
            session_expires_secs: timer.map(|t| t.interval_secs),
        };
        self.dialogs.insert(dialog_key.clone(), record.clone());
        self.replicator
            .replicate(smiths_core::DialogDelta::Upsert(Box::new(record)));
        self.metrics.dialogs_active.inc();
        if let Some(m) = &media {
            self.leg_media.insert(
                dialog_key.clone(),
                LegMedia::from_offer(m.endpoint.id(), &m.offer),
            );
        }

        // CDR: remember the call's start + URIs so termination can
        // emit a complete record.
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
        let timer_headers = session_timer_headers(timer);
        extras.extend(timer_headers.iter().map(|(n, v)| (*n, v.as_str())));
        if media.is_some() {
            extras.push(("Content-Type", "application/sdp"));
        }
        let body = media
            .as_ref()
            .map(|m| m.offer.answer_body.as_str())
            .unwrap_or_default();
        self.respond(
            req,
            200,
            "OK",
            Some(&local_tag),
            &extras,
            body.as_bytes(),
            peer,
        )
        .await;
        info!(%call_id, ?attachment, "dialog established");

        if let Some(t) = timer {
            self.arm_session_timer(&dialog_key, t.interval_secs);
        }
        self.arm_duration_guard(&dialog_key);

        let _ = self.bus.publish(Event::Sip(SipEvent::DialogCreated {
            call_id,
            media_endpoint: media.as_ref().map(|m| m.endpoint.id()),
            remote_rtp: media.as_ref().and_then(|m| m.offer.remote_media),
        }));
    }

    /// RFC 4028 §9 negotiation for an INVITE / UPDATE. `Ok(None)` =
    /// no session timer on this dialog (timers disabled, peer lacks
    /// `Supported: timer`, or peer insists on `refresher=uas`, which
    /// this UAS does not act as). `Err(min_se)` = the requested
    /// interval is below the floor; answer `422` with that `Min-SE`.
    fn negotiate_session_timer(
        &self,
        req: &RequestSummary,
    ) -> Result<Option<SessionTimerAgreement>, u32> {
        let cfg = &self.session_timer;
        if !cfg.enabled || !req.supports_timer {
            return Ok(None);
        }
        let min_se = secs_u32(cfg.min_se);
        let requested = match req.session_expires {
            Some(se) if se < min_se => return Err(min_se),
            Some(se) => {
                if req.session_refresher.as_deref() == Some("uas") {
                    debug!("peer requires refresher=uas; running without session timer");
                    return Ok(None);
                }
                se
            }
            None => secs_u32(cfg.default_expires),
        };
        let interval_secs = requested.max(req.min_se.unwrap_or(0)).max(min_se);
        Ok(Some(SessionTimerAgreement { interval_secs }))
    }

    async fn respond_interval_too_small(
        &self,
        req: &RequestSummary,
        peer: SocketAddr,
        min_se: u32,
    ) {
        let min_se = min_se.to_string();
        info!(%peer, %min_se, "Session-Expires below Min-SE; 422");
        self.respond(
            req,
            422,
            "Session Interval Too Small",
            Some(&self.take_invite_tag(req)),
            &[("Min-SE", min_se.as_str())],
            b"",
            peer,
        )
        .await;
    }

    /// `true` when a CANCEL for this INVITE was accepted at ingress
    /// and the INVITE has not yet sent its final response.
    fn invite_was_cancelled(&self, req: &RequestSummary) -> bool {
        req.branch
            .as_deref()
            .is_some_and(|b| self.cancelled_invites.contains_key(b))
    }

    /// The To-tag for this INVITE's final response: the tag the
    /// `200 OK` to a racing CANCEL already used (RFC 3261 §9.2 wants
    /// the two to match), otherwise a fresh one. Consumes the CANCEL
    /// flag, so it is the last thing to call before the final goes out.
    fn take_invite_tag(&self, req: &RequestSummary) -> String {
        req.branch
            .as_deref()
            .and_then(|b| self.cancelled_invites.remove(b))
            .map_or_else(next_tag, |(_, tag)| tag)
    }

    /// `487 Request Terminated` for an INVITE whose CANCEL was
    /// accepted while it was still pending.
    async fn reject_cancelled_invite(&self, req: &RequestSummary, peer: SocketAddr) {
        let tag = self.take_invite_tag(req);
        info!(%peer, "INVITE cancelled before its final response; 487");
        self.respond(req, 487, "Request Terminated", Some(&tag), &[], b"", peer)
            .await;
    }

    // -----------------------------------------------------------------
    // In-dialog requests
    // -----------------------------------------------------------------

    /// Re-INVITE (RFC 3261 §14.2): re-run offer/answer against the
    /// dialog's existing endpoint — codec changes and hold
    /// (`sendonly` → `recvonly`, `inactive` → `inactive`) come out of
    /// the negotiator's direction handling — refresh the RFC 4028
    /// timer, and answer `200 OK`. A re-INVITE without an offer gets
    /// the engine's current offer in the 2xx.
    #[instrument(skip_all, fields(%peer, call_id = %req.call_id.as_deref().unwrap_or("-")))]
    async fn handle_reinvite(&self, req: &RequestSummary, peer: SocketAddr) {
        let Some(key) = in_dialog_key(req) else {
            self.respond(req, 400, "Bad Request", Some(&next_tag()), &[], b"", peer)
                .await;
            return;
        };
        let Some(record) = self.dialogs.get(&key).map(|r| r.clone()) else {
            self.respond_481(req, peer).await;
            return;
        };
        if !self.in_dialog_cseq_ok(req, &record, peer).await {
            return;
        }
        let timer = match self.negotiate_session_timer(req) {
            Ok(t) => t,
            Err(min_se) => {
                self.respond_interval_too_small(req, peer, min_se).await;
                return;
            }
        };
        let Ok(renegotiation) = self.renegotiate(req, peer, &key, &record).await else {
            return;
        };
        self.commit_in_dialog_update(&key, req, &renegotiation, timer);

        let mut extras: Vec<(&str, &str)> = vec![("Contact", self.contact.as_str())];
        let timer_headers = session_timer_headers(timer);
        extras.extend(timer_headers.iter().map(|(n, v)| (*n, v.as_str())));
        if !renegotiation.body.is_empty() {
            extras.push(("Content-Type", "application/sdp"));
        }
        self.respond(
            req,
            200,
            "OK",
            Some(&record.local_tag),
            &extras,
            renegotiation.body.as_bytes(),
            peer,
        )
        .await;
        info!(call_id = %record.call_id, "re-INVITE answered");
        self.rearm_session_timer(&key, timer);
    }

    /// UPDATE (RFC 3311): an offer is renegotiated like a re-INVITE;
    /// a bodiless UPDATE is an RFC 4028 refresh. Answered through the
    /// non-INVITE server transaction (no ACK, no 2xx retransmit loop).
    #[instrument(skip_all, fields(%peer, call_id = %req.call_id.as_deref().unwrap_or("-")))]
    async fn handle_update(&self, req: &RequestSummary, peer: SocketAddr) {
        let Some(key) = in_dialog_key(req) else {
            self.respond_481(req, peer).await;
            return;
        };
        let Some(record) = self.dialogs.get(&key).map(|r| r.clone()) else {
            self.respond_481(req, peer).await;
            return;
        };
        if !self.in_dialog_cseq_ok(req, &record, peer).await {
            return;
        }
        let timer = match self.negotiate_session_timer(req) {
            Ok(t) => t,
            Err(min_se) => {
                self.respond_interval_too_small(req, peer, min_se).await;
                return;
            }
        };
        let renegotiation = if req.has_sdp() {
            match self.renegotiate(req, peer, &key, &record).await {
                Ok(r) => r,
                Err(()) => return,
            }
        } else {
            Renegotiation {
                body: String::new(),
                offer: None,
                new_endpoint: None,
            }
        };
        self.commit_in_dialog_update(&key, req, &renegotiation, timer);

        let mut extras: Vec<(&str, &str)> = vec![("Contact", self.contact.as_str())];
        let timer_headers = session_timer_headers(timer);
        extras.extend(timer_headers.iter().map(|(n, v)| (*n, v.as_str())));
        if !renegotiation.body.is_empty() {
            extras.push(("Content-Type", "application/sdp"));
        }
        self.respond(
            req,
            200,
            "OK",
            Some(&record.local_tag),
            &extras,
            renegotiation.body.as_bytes(),
            peer,
        )
        .await;
        debug!(call_id = %record.call_id, "UPDATE answered");
        self.rearm_session_timer(&key, timer);
    }

    /// RFC 3261 §12.2.2 remote sequence check. Answers `400` when the
    /// request has no usable `CSeq` and `500` when it is out of
    /// order; `true` means the request may proceed.
    async fn in_dialog_cseq_ok(
        &self,
        req: &RequestSummary,
        record: &DialogRecord,
        peer: SocketAddr,
    ) -> bool {
        let Some(cseq) = req.cseq else {
            self.respond(
                req,
                400,
                "Bad Request",
                Some(&record.local_tag),
                &[],
                b"",
                peer,
            )
            .await;
            return false;
        };
        if let Some(prev) = record.remote_cseq
            && cseq <= prev
        {
            warn!(%peer, cseq, prev, "in-dialog request out of order; 500");
            self.respond(
                req,
                500,
                "Server Internal Error",
                Some(&record.local_tag),
                &[],
                b"",
                peer,
            )
            .await;
            return false;
        }
        true
    }

    /// Re-run offer/answer for an in-dialog request. With an SDP
    /// offer, the dialog's existing endpoint (or a fresh one for a
    /// dialog that started media-less) answers it and the bridge is
    /// rebuilt if the peer's RTP address or keys changed. Without an
    /// offer, the 2xx carries the engine's own offer (§14.2). Every
    /// failure has been answered before `Err` comes back.
    async fn renegotiate(
        &self,
        req: &RequestSummary,
        peer: SocketAddr,
        key: &DialogKey,
        record: &DialogRecord,
    ) -> Result<Renegotiation, ()> {
        let local_ip = self.effective_local_ip(peer).await;
        if !req.has_sdp() {
            let body = record
                .local_media
                .map(|local| self.negotiator.build_offer(local_ip, local.port()))
                .unwrap_or_default();
            return Ok(Renegotiation {
                body,
                offer: None,
                new_endpoint: None,
            });
        }
        let (endpoint_id, port, new_endpoint) = match (record.media, record.local_media) {
            (Some(id), Some(local)) => (id, local.port(), None),
            _ => match self.media_fabric.allocate(self.media_bind_ip).await {
                Ok(ep) => (ep.id(), ep.local_addr().port(), Some(ep)),
                Err(e) => {
                    warn!(?e, "failed to allocate media endpoint for re-INVITE");
                    self.respond(
                        req,
                        500,
                        "Server Internal Error",
                        Some(&record.local_tag),
                        &[],
                        b"",
                        peer,
                    )
                    .await;
                    return Err(());
                }
            },
        };
        let body = req.body.as_deref().unwrap_or_default();
        match self.negotiate_body(body, local_ip, port) {
            Ok(offer) => {
                self.apply_leg_media(key, endpoint_id, &offer).await;
                Ok(Renegotiation {
                    body: offer.answer_body.clone(),
                    offer: Some(offer),
                    new_endpoint,
                })
            }
            Err(e) => {
                if let Some(ep) = new_endpoint {
                    self.media_fabric.release_endpoint(ep.id()).await;
                }
                self.reject_offer(req, peer, &record.local_tag, e).await;
                Err(())
            }
        }
    }

    /// Store the renegotiated media view for `key` and, when the
    /// peer's RTP address or SRTP keys moved and the dialog is part
    /// of a passthrough bridge, rebuild that bridge.
    async fn apply_leg_media(&self, key: &DialogKey, endpoint: EndpointId, offer: &AcceptedOffer) {
        let new = LegMedia::from_offer(endpoint, offer);
        let changed = self.leg_media.get(key).is_none_or(|old| *old != new);
        self.leg_media.insert(key.clone(), new.clone());
        if !changed {
            return;
        }
        let Some(bid) = self.bridges_by_dialog.get(key).map(|b| *b) else {
            if !self.dialog_sessions.remove_dialog(key).is_empty() {
                // Handles were only inspected, not stopped: the
                // session keeps running against the previous address.
                warn!(
                    ?key,
                    "peer media moved on a session-backed leg; session not re-pointed"
                );
            }
            return;
        };
        let Some(peer_key) = self
            .bridges_by_dialog
            .iter()
            .find(|e| *e.value() == bid && e.key() != key)
            .map(|e| e.key().clone())
        else {
            return;
        };
        let Some(peer_leg) = self.leg_media.get(&peer_key).map(|l| l.clone()) else {
            return;
        };
        let (Some(leg_a), Some(leg_b)) = (new.bridge_leg(), peer_leg.bridge_leg()) else {
            return;
        };
        self.media_fabric.release_bridge(bid).await;
        match self.media_fabric.bridge(leg_a, leg_b).await {
            Ok(nb) => {
                self.bridges_by_dialog.insert(key.clone(), nb);
                self.bridges_by_dialog.insert(peer_key, nb);
                info!(?key, "bridge rebuilt after re-INVITE");
            }
            Err(e) => {
                self.bridges_by_dialog.remove(key);
                self.bridges_by_dialog.remove(&peer_key);
                warn!(?key, ?e, "bridge rebuild after re-INVITE failed");
            }
        }
    }

    /// Fold an accepted in-dialog request into the dialog record:
    /// remote `CSeq`, INVITE branch, media fields, session interval.
    fn commit_in_dialog_update(
        &self,
        key: &DialogKey,
        req: &RequestSummary,
        renegotiation: &Renegotiation,
        timer: Option<SessionTimerAgreement>,
    ) {
        let Some(mut entry) = self.dialogs.get_mut(key) else {
            return;
        };
        entry.remote_cseq = req.cseq;
        if req.method == "INVITE" {
            entry.last_invite_branch.clone_from(&req.branch);
        }
        entry.session_expires_secs = timer.map(|t| t.interval_secs);
        if let Some(ep) = &renegotiation.new_endpoint {
            entry.media = Some(ep.id());
            entry.local_media = Some(ep.local_addr());
        }
        if let Some(offer) = &renegotiation.offer {
            entry.remote_media = offer.remote_media;
            entry.ice.clone_from(&offer.ice);
            entry.per_leg_codec.clear();
            if let Some(c) = offer.audio_codec.clone() {
                entry.per_leg_codec.insert(smiths_core::LegId(0), c);
            }
            if let Some(c) = offer.video_codec.clone() {
                entry.per_leg_codec.insert(smiths_core::LegId(1), c);
            }
        }
        let snapshot = entry.clone();
        drop(entry);
        self.replicator
            .replicate(smiths_core::DialogDelta::Upsert(Box::new(snapshot)));
    }

    /// Refresh (or drop) the RFC 4028 timer after a re-INVITE / UPDATE.
    fn rearm_session_timer(&self, key: &DialogKey, timer: Option<SessionTimerAgreement>) {
        match timer {
            Some(t) => self.arm_session_timer(key, t.interval_secs),
            None => self.cancel_session_timer(key),
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

        // Dialog-layer: Early → Confirmed on the first 2xx ACK; an ACK
        // for a re-INVITE 2xx leaves the state alone. Either way the
        // §13.3.1.4 retransmit loop stops and the parked 2xx bytes go.
        let Some(key) = in_dialog_key(req) else {
            debug!(%peer, "ACK missing dialog identifiers; dropping");
            return;
        };
        if let Some(mut entry) = self.dialogs.get_mut(&key) {
            entry.pending_2xx = None;
            if entry.state == DialogState::Early
                && drive_dialog_fsm(&mut entry, DialogEvent::AckReceived) == FsmOutcome::Continue
            {
                info!(call_id = %entry.call_id, "dialog confirmed");
            }
            let snapshot = entry.clone();
            drop(entry);
            self.replicator
                .replicate(smiths_core::DialogDelta::Upsert(Box::new(snapshot)));
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
        match self
            .teardown_call(&key, false, DialogEvent::ByeCompleted)
            .await
        {
            Some(record) => {
                self.respond(req, 200, "OK", Some(&record.local_tag), &[], &[], peer)
                    .await;
                // CDR fires after the 200 lands so a slow
                // `CdrStore::record` never delays the BYE response.
                self.finish_teardown(&key, &record, "answered");
            }
            None => self.respond_481(req, peer).await,
        }
    }

    /// CANCEL whose INVITE server transaction is already gone: a
    /// dialog that answered that INVITE with a 2xx gets `200 OK` and
    /// keeps going (the caller ACKs and BYEs per §15.1.1); anything
    /// else is `481` (§9.2).
    async fn handle_cancel(&self, req: &RequestSummary, peer: SocketAddr) {
        let (Some(branch), Some(call_id)) = (req.branch.as_deref(), req.call_id.as_deref()) else {
            self.respond(req, 400, "Bad Request", Some(&next_tag()), &[], b"", peer)
                .await;
            return;
        };
        let answered = self
            .dialogs
            .iter()
            .find(|e| e.key().0 == call_id && e.last_invite_branch.as_deref() == Some(branch))
            .map(|e| e.local_tag.clone());
        match answered {
            Some(tag) => {
                debug!(%peer, branch, "CANCEL for an INVITE already answered 2xx; no effect");
                self.respond(req, 200, "OK", Some(&tag), &[], b"", peer)
                    .await;
            }
            None => self.respond_481(req, peer).await,
        }
    }

    async fn respond_481(&self, req: &RequestSummary, peer: SocketAddr) {
        self.respond(
            req,
            481,
            "Call/Transaction Does Not Exist",
            Some(&next_tag()),
            &[],
            b"",
            peer,
        )
        .await;
    }

    // -----------------------------------------------------------------
    // Teardown
    // -----------------------------------------------------------------

    /// Remove one dialog and release everything it owned: timers, the
    /// parked rendezvous leg, its bridge, sessions, media. With
    /// `send_bye` an in-dialog BYE goes to the peer. Returns the
    /// record plus the key of the leg that shared its bridge, if any;
    /// the caller ends that one too.
    async fn teardown_dialog(
        &self,
        key: &DialogKey,
        send_bye: bool,
        event: DialogEvent,
    ) -> Option<TornDown> {
        let (_, mut record) = self.dialogs.remove(key)?;
        if drive_dialog_fsm(&mut record, event) != FsmOutcome::Terminated {
            warn!(call_id = %record.call_id, ?event, "dialog FSM did not reach Terminated; record dropped anyway");
        }
        self.metrics.dialogs_active.dec();
        self.replicator
            .replicate(smiths_core::DialogDelta::Delete(key.clone()));
        self.cancel_invite_2xx_retransmit(key);
        self.cancel_dialog_timers(key);
        self.leg_media.remove(key);
        // Drop an unpaired pending leg if this was it.
        if let Some(rv) = record.rendezvous.as_ref() {
            self.pending_bridges
                .remove_if(rv, |_, pending| pending.dialog_key == *key);
        }
        // Tear down the live bridge if this dialog is part of one.
        // Whichever side ends first wins the race; the second finds
        // no entry and the fabric release is idempotent.
        let mut bridged_peer = None;
        if let Some((_, bid)) = self.bridges_by_dialog.remove(key) {
            bridged_peer = self
                .bridges_by_dialog
                .iter()
                .find(|e| *e.value() == bid)
                .map(|e| e.key().clone());
            self.bridges_by_dialog.retain(|_, other| *other != bid);
            self.media_fabric.release_bridge(bid).await;
            debug!(call_id = %record.call_id, "rendezvous bridge stopped");
        }
        // Drain any non-passthrough sessions (transcoded, conference)
        // that belong to this dialog so the forwarder tasks exit and
        // any admission lease releases.
        for session in self.dialog_sessions.remove_dialog(key) {
            session.stop().await;
        }
        if let Some(ep) = record.media {
            self.media_fabric.release_endpoint(ep).await;
        }
        if send_bye {
            self.send_in_dialog_bye(&mut record).await;
        }
        Some(TornDown {
            record,
            bridged_peer,
        })
    }

    /// End a call: the dialog `key` and, when it was bridged, the leg
    /// on the other side of the bridge (which always gets a BYE — a
    /// released media bridge would otherwise leave that peer's dialog
    /// up, hearing silence). Returns `key`'s record; the caller emits
    /// its CDR / event via [`Self::finish_teardown`] once any pending
    /// response has gone out.
    async fn teardown_call(
        &self,
        key: &DialogKey,
        send_bye: bool,
        event: DialogEvent,
    ) -> Option<DialogRecord> {
        let torn = self.teardown_dialog(key, send_bye, event).await?;
        if let Some(peer_key) = torn.bridged_peer
            && let Some(peer_torn) = self
                .teardown_dialog(&peer_key, true, DialogEvent::ByeCompleted)
                .await
        {
            self.finish_teardown(&peer_key, &peer_torn.record, "answered");
        }
        Some(torn.record)
    }

    /// End a call on the engine's initiative (timer expiry): BYE to
    /// the peer, media released, CDR + event emitted.
    async fn end_call(&self, key: &DialogKey, event: DialogEvent, result: &str) {
        if let Some(record) = self.teardown_call(key, true, event).await {
            self.finish_teardown(key, &record, result);
        }
    }

    /// Emit the CDR row and the `DialogTerminated` event for a dialog
    /// [`Self::teardown_dialog`] already removed.
    fn finish_teardown(&self, key: &DialogKey, record: &DialogRecord, result: &str) {
        self.emit_cdr_for(key, result);
        let _ = self.bus.publish(Event::Sip(SipEvent::DialogTerminated {
            call_id: record.call_id.clone(),
        }));
    }

    /// Originate an in-dialog BYE toward `record`'s peer (RFC 3261
    /// §15.1.2 / §12.2.1.1): Request-URI from the remote target,
    /// `Route` from the route set, `From` / `To` mirroring the
    /// dialog's own URIs and tags, the next local `CSeq`, and a `Via`
    /// naming the transport the dialog arrived on. The request runs
    /// through a client non-INVITE transaction, so it is retransmitted
    /// per timer E and the peer's answer is logged.
    async fn send_in_dialog_bye(&self, record: &mut DialogRecord) {
        record.local_cseq = record.local_cseq.saturating_add(1);
        let dest = record.peer_signal;
        let bound = self.transport.local_addr().unwrap_or(dest);
        let via_sent_by = SocketAddr::new(
            crate::transport::resolve_local_ip_for(bound.ip(), dest).await,
            bound.port(),
        );
        let branch = format!("z9hG4bK-{}", next_tag());
        let transport = record
            .transport
            .clone()
            .unwrap_or_else(|| self.transport.kind().via_token().to_owned());
        let bye = build_in_dialog_request(&InDialogRequest {
            method: "BYE",
            record,
            via_sent_by,
            via_transport: &transport,
            branch: &branch,
        });
        let txn = ClientNonInviteTxn::new(branch.clone(), "BYE", Bytes::from(bye));
        let mut tu_rx = self.txn_driver.start_client(Box::new(txn), dest);
        debug!(call_id = %record.call_id, peer = %dest, branch, "BYE → dialog peer");
        let call_id = record.call_id.clone();
        tokio::spawn(async move {
            loop {
                match tu_rx.recv().await {
                    Some(TuEvent::Response { status, .. }) if status >= 200 => {
                        info!(%call_id, peer = %dest, status, "engine BYE answered");
                        break;
                    }
                    Some(TuEvent::Response { .. }) => {}
                    Some(TuEvent::Terminated) | None => {
                        warn!(%call_id, peer = %dest, "engine BYE got no final response");
                        break;
                    }
                }
            }
        });
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
            debug!(?key, "dialog ended without cdr_pending — no row emitted");
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

    // -----------------------------------------------------------------
    // Responses
    // -----------------------------------------------------------------

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
                let snapshot = entry.clone();
                drop(entry);
                self.replicator
                    .replicate(smiths_core::DialogDelta::Upsert(Box::new(snapshot)));
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

    // -----------------------------------------------------------------
    // Dialog timers
    // -----------------------------------------------------------------

    /// Start the RFC 3261 §13.3.1.4 per-dialog 2xx retransmit loop.
    /// The first retransmit fires at T1 after this call (the initial
    /// send goes through the FSM's `SendToPeer` in [`Self::respond`]);
    /// subsequent intervals double up to T2 and the whole loop caps
    /// at 64·T1 total wall-clock, after which an
    /// [`InternalEvent::AckTimeout`] asks the dialog's worker to end
    /// the call with a BYE. ACK (cancel token tripped in
    /// [`Self::handle_ack`]) or dialog teardown bow out early.
    ///
    /// Each retransmit bumps `sip_invite_2xx_retransmits`. A retransmit
    /// emitted after the first is the operator's signal that either
    /// (a) the 2xx was lost and we're doing RFC-compliant recovery,
    /// or (b) the peer stopped `ACK`ing and we're burning the budget.
    fn spawn_invite_2xx_retransmit(&self, key: &DialogKey, bytes: Bytes, peer: SocketAddr) {
        let cancel = CancellationToken::new();
        // Replace any pre-existing handle for this dialog (a
        // re-INVITE 2xx while the previous one is still unACKed).
        if let Some(old) = self
            .invite_2xx_retransmits
            .insert(key.clone(), cancel.clone())
        {
            old.cancel();
        }
        let transport = Arc::clone(&self.transport);
        let metrics = Arc::clone(&self.metrics);
        let retransmits = Arc::clone(&self.invite_2xx_retransmits);
        let internal_tx = self.internal_tx.clone();
        let budget = self.invite_2xx_timeout;
        let task_key = key.clone();
        let task_cancel = cancel.clone();
        tokio::spawn(async move {
            let mut interval = T1;
            let mut elapsed = Duration::ZERO;
            let mut ack_timed_out = false;
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
                if elapsed > budget {
                    warn!(
                        ?task_key,
                        "INVITE 2xx retransmit budget exhausted without ACK"
                    );
                    ack_timed_out = true;
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
                if ack_timed_out {
                    let _ = internal_tx.send(InternalEvent::AckTimeout { key: task_key });
                }
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

    /// Arm (or re-arm) the RFC 4028 expiry for `key`. Fires
    /// [`InternalEvent::SessionExpired`] at `interval − min(32 s,
    /// interval / 3)` (§10) unless a refresh re-arms it first.
    fn arm_session_timer(&self, key: &DialogKey, interval_secs: u32) {
        let generation = self
            .session_timers
            .get(key)
            .map_or(0, |t| t.generation)
            .wrapping_add(1);
        let cancel = CancellationToken::new();
        if let Some(old) = self.session_timers.insert(
            key.clone(),
            SessionTimer {
                cancel: cancel.clone(),
                generation,
            },
        ) {
            old.cancel.cancel();
        }
        let interval = Duration::from_secs(u64::from(interval_secs));
        let headroom = std::cmp::min(SESSION_EXPIRY_HEADROOM_MAX, interval / 3);
        let fire_after = interval.saturating_sub(headroom);
        let tx = self.internal_tx.clone();
        let key = key.clone();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {}
                () = tokio::time::sleep(fire_after) => {
                    let _ = tx.send(InternalEvent::SessionExpired { key, generation });
                }
            }
        });
    }

    fn cancel_session_timer(&self, key: &DialogKey) {
        if let Some((_, timer)) = self.session_timers.remove(key) {
            timer.cancel.cancel();
        }
    }

    /// Arm the absolute call-duration guard for `key`, if configured.
    fn arm_duration_guard(&self, key: &DialogKey) {
        let Some(max) = self.max_call_duration else {
            return;
        };
        let cancel = CancellationToken::new();
        if let Some(old) = self.duration_guards.insert(key.clone(), cancel.clone()) {
            old.cancel();
        }
        let tx = self.internal_tx.clone();
        let key = key.clone();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {}
                () = tokio::time::sleep(max) => {
                    let _ = tx.send(InternalEvent::MaxDurationReached { key });
                }
            }
        });
    }

    fn cancel_dialog_timers(&self, key: &DialogKey) {
        self.cancel_session_timer(key);
        if let Some((_, token)) = self.duration_guards.remove(key) {
            token.cancel();
        }
    }
}

/// Result of a digest-auth run.
enum AuthOutcome {
    /// Credentials verified; carries the username.
    Authenticated(String),
    /// Send `401` with this `WWW-Authenticate` value.
    Challenge(String),
    /// The credential store itself failed (panicked lookup); `500`.
    Failed,
}

/// Outcome of re-running offer/answer for an in-dialog request.
struct Renegotiation {
    /// Body for the 2xx: the SDP answer, the engine's own offer for a
    /// bodiless re-INVITE, or empty.
    body: String,
    /// The accepted offer, when the request carried one.
    offer: Option<AcceptedOffer>,
    /// Endpoint allocated for a dialog that started without media.
    new_endpoint: Option<Arc<dyn smiths_core::media::MediaEndpoint>>,
}

/// What [`UasServer::teardown_dialog`] hands back.
struct TornDown {
    record: DialogRecord,
    /// The dialog on the other side of the released bridge, if any.
    bridged_peer: Option<DialogKey>,
}

/// Result of feeding one event to a dialog's FSM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FsmOutcome {
    /// Legal transition into a live state; `record.state` updated.
    Continue,
    /// Legal transition into Terminated; the record must be dropped.
    Terminated,
    /// The event is not legal in the record's state; logged, record
    /// untouched.
    Illegal,
}

/// Run `event` through a [`DialogFsm`] rebuilt from `record.state`
/// and project the result back onto the record. Illegal transitions
/// are logged at warn level and leave the record as it was.
fn drive_dialog_fsm(record: &mut DialogRecord, event: DialogEvent) -> FsmOutcome {
    let mut fsm = DialogFsm::from_core_state(record.state);
    match fsm.on_event(event) {
        Ok(_) => match fsm.to_core_state() {
            Some(state) => {
                record.state = state;
                FsmOutcome::Continue
            }
            None => FsmOutcome::Terminated,
        },
        Err(e) => {
            warn!(call_id = %record.call_id, %e, "illegal dialog transition ignored");
            FsmOutcome::Illegal
        }
    }
}

/// Wait for the next timer event, or forever when the receiver has
/// already been taken (only possible if `run` were entered twice).
async fn recv_internal(rx: &mut Option<mpsc::UnboundedReceiver<InternalEvent>>) -> InternalEvent {
    match rx.as_mut() {
        Some(rx) => match rx.recv().await {
            Some(ev) => ev,
            // Every sender lives in `UasServer`, which outlives the
            // loop, so the channel cannot close while `run` polls it.
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
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
/// `<code> <warn-agent> "<text>"`. `warn-agent` is our product token,
/// and the text is quoted so spaces inside it survive the wire. Any
/// embedded `"` is stripped — the text may originate from internal
/// error strings but ends up on the wire.
fn format_warning(code: u16, text: &str) -> String {
    let sanitized: String = text.chars().filter(|c| *c != '"').collect();
    format!("{code} smiths-net \"{sanitized}\"")
}

/// `Session-Expires` + `Require: timer` for a 2xx that agreed on a
/// session interval (RFC 4028 §9). Empty when no timer applies.
fn session_timer_headers(timer: Option<SessionTimerAgreement>) -> Vec<(&'static str, String)> {
    match timer {
        Some(t) => vec![
            (
                "Session-Expires",
                format!("{};refresher=uac", t.interval_secs),
            ),
            ("Require", "timer".to_owned()),
        ],
        None => Vec::new(),
    }
}

/// Whole seconds of `d`, saturating at `u32::MAX`.
fn secs_u32(d: Duration) -> u32 {
    u32::try_from(d.as_secs()).unwrap_or(u32::MAX)
}

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

/// `branch` parameter of one `Via` header value (everything after
/// the colon). `None` when the parameter is absent or empty.
fn via_branch_param(value: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    let idx = lower.find(";branch=")?;
    let after = &value[idx + ";branch=".len()..];
    let end = after
        .find(|c: char| c == ';' || c == ',' || c.is_whitespace())
        .unwrap_or(after.len());
    (end > 0).then(|| after[..end].to_owned())
}

/// Extract the first `Via` header's `branch` parameter from any raw
/// SIP message (request or response). Returns `None` when the header
/// or parameter is missing. Used to correlate inbound responses.
fn extract_via_branch(raw: &Bytes) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    for line in text.split("\r\n").skip(1) {
        if line.is_empty() {
            break; // headers done
        }
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("via") || name.trim().eq_ignore_ascii_case("v") {
            return via_branch_param(value);
        }
    }
    None
}

/// Numeric status code of a raw SIP response, from its first line.
fn response_status(bytes: &[u8]) -> Option<u16> {
    let end = bytes.iter().position(|&b| b == b'\r')?;
    let line = std::str::from_utf8(&bytes[..end]).ok()?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Key for an in-dialog request (ACK, BYE, re-INVITE, UPDATE).
///
/// An incoming request sees `From` as remote and `To` as local, so
/// the key is `(Call-ID, To-tag, From-tag)`.
fn in_dialog_key(req: &RequestSummary) -> Option<DialogKey> {
    let call_id = req.call_id.clone()?;
    let local_tag = req.to_tag.clone()?;
    let remote_tag = req.from_tag.clone()?;
    Some((call_id, local_tag, remote_tag))
}

/// Extract the routing info we need from a request's raw bytes.
#[allow(clippy::too_many_lines)] // one pass over the header block, one arm per header
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
    let mut cseq = None;
    let mut content_type: Option<String> = None;
    let mut authorization: Option<String> = None;
    let mut contact: Option<String> = None;
    let mut record_route: Vec<String> = Vec::new();
    let mut expires: Option<u32> = None;
    let mut from_uri: Option<String> = None;
    let mut to_uri: Option<String> = None;
    let mut session_expires: Option<u32> = None;
    let mut session_refresher: Option<String> = None;
    let mut min_se: Option<u32> = None;
    let mut supports_timer = false;
    let mut webrtc_tag: Option<String> = None;

    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        // First occurrence of each header wins; `Record-Route` and
        // `Supported` accumulate.
        match name.as_str() {
            "via" | "v" if branch.is_none() => branch = via_branch_param(value),
            "call-id" | "i" if call_id.is_none() && !value.is_empty() => {
                call_id = Some(value.to_owned());
            }
            "from" | "f" if from_tag.is_none() => {
                from_tag = extract_tag_param(value);
                from_uri = first_contact_uri(value);
            }
            "to" | "t" if to_tag.is_none() => {
                to_tag = extract_tag_param(value);
                to_uri = first_contact_uri(value);
            }
            "cseq" if cseq.is_none() => {
                cseq = value.split_whitespace().next().and_then(|n| n.parse().ok());
            }
            "content-type" | "c" if content_type.is_none() => {
                // Strip any `; charset=...` and normalize.
                let media_type = value
                    .split(';')
                    .next()
                    .unwrap_or(value)
                    .trim()
                    .to_ascii_lowercase();
                if !media_type.is_empty() {
                    content_type = Some(media_type);
                }
            }
            "authorization" if authorization.is_none() && !value.is_empty() => {
                authorization = Some(value.to_owned());
            }
            "contact" | "m" if contact.is_none() && !value.is_empty() => {
                contact = Some(value.to_owned());
            }
            "record-route" => record_route.extend(split_header_list(value)),
            "expires" if expires.is_none() => expires = value.parse::<u32>().ok(),
            "session-expires" | "x" if session_expires.is_none() => {
                let (secs, refresher) = parse_session_expires(value);
                session_expires = secs;
                session_refresher = refresher;
            }
            "min-se" if min_se.is_none() => {
                min_se = value.split(';').next().and_then(|n| n.trim().parse().ok());
            }
            "supported" | "k" => {
                supports_timer |= split_header_list(value)
                    .iter()
                    .any(|tok| tok.eq_ignore_ascii_case("timer"));
            }
            "x-smiths-webrtc-tag" if webrtc_tag.is_none() && !value.is_empty() => {
                webrtc_tag = Some(value.to_owned());
            }
            _ => {}
        }
    }

    RequestSummary {
        method,
        branch,
        call_id,
        from_tag,
        to_tag,
        cseq,
        ruri_user,
        request_uri,
        authorization,
        content_type,
        contact,
        record_route,
        from_uri,
        to_uri,
        expires,
        session_expires,
        session_refresher,
        min_se,
        supports_timer,
        body: (!body.is_empty()).then(|| body.to_owned()),
        raw: raw.clone(),
        webrtc_tag,
    }
}

/// `Session-Expires: 1800;refresher=uac` → `(Some(1800), Some("uac"))`.
fn parse_session_expires(value: &str) -> (Option<u32>, Option<String>) {
    let mut parts = value.split(';');
    let secs = parts.next().and_then(|n| n.trim().parse().ok());
    let refresher = parts.find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("refresher")
            .then(|| v.trim().to_ascii_lowercase())
    });
    (secs, refresher)
}

/// Split a comma-separated header value into its elements, leaving
/// commas inside `<...>` (URI parameters, headers) alone.
fn split_header_list(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for c in value.chars() {
        match c {
            '<' => {
                depth += 1;
                current.push(c);
            }
            '>' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            ',' if depth == 0 => {
                let item = current.trim();
                if !item.is_empty() {
                    out.push(item.to_owned());
                }
                current.clear();
            }
            _ => current.push(c),
        }
    }
    let item = current.trim();
    if !item.is_empty() {
        out.push(item.to_owned());
    }
    out
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
/// Deliberately permissive — the registrar binds one URI per AOR;
/// multiple contacts + `;expires=N` per-contact params (RFC 3261
/// §25.1) are not parsed.
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
/// - Emits `Content-Length: N` derived from `body.len` and appends
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

/// Fields for an engine-originated in-dialog request.
struct InDialogRequest<'a> {
    method: &'a str,
    /// Dialog the request belongs to; `local_cseq` must already hold
    /// the sequence number to use.
    record: &'a DialogRecord,
    via_sent_by: SocketAddr,
    /// `Via` transport token (`UDP`, `TCP`, `TLS`).
    via_transport: &'a str,
    branch: &'a str,
}

/// Request-URI and `Route` header values for a request inside
/// `record`'s dialog (RFC 3261 §12.2.1.1). With an empty route set
/// the request goes straight to the remote target. When the first
/// route is a loose router (`;lr`) the Request-URI stays the remote
/// target and the route set is copied into `Route`. A strict router
/// first becomes the Request-URI itself, with the remaining routes
/// plus the remote target in `Route`.
fn dialog_route_target(record: &DialogRecord) -> (String, Vec<String>) {
    let remote_target = record
        .remote_target
        .clone()
        .unwrap_or_else(|| format!("sip:{}", record.peer_signal));
    let Some(first) = record.route_set.first() else {
        return (remote_target, Vec::new());
    };
    let loose = first.to_ascii_lowercase().contains(";lr");
    if loose {
        return (remote_target, record.route_set.clone());
    }
    let mut routes: Vec<String> = record.route_set.iter().skip(1).cloned().collect();
    routes.push(format!("<{remote_target}>"));
    let first_uri = first_contact_uri(first).unwrap_or_else(|| first.clone());
    (first_uri, routes)
}

/// Build an RFC 3261 in-dialog request (BYE today) from the dialog
/// record: Request-URI + `Route` per [`dialog_route_target`], `From`
/// = the engine's own URI and tag, `To` = the peer's URI and tag,
/// `CSeq` = the record's local sequence number.
fn build_in_dialog_request(f: &InDialogRequest<'_>) -> Vec<u8> {
    let record = f.record;
    let (request_uri, routes) = dialog_route_target(record);
    let local_uri = record
        .local_uri
        .clone()
        .unwrap_or_else(|| format!("sip:smiths@{}", f.via_sent_by));
    let remote_uri = record
        .remote_uri
        .clone()
        .unwrap_or_else(|| format!("sip:{}", record.peer_signal));
    let mut out = String::with_capacity(512);
    let _ = write!(out, "{} {request_uri} SIP/2.0\r\n", f.method);
    let _ = write!(
        out,
        "Via: SIP/2.0/{} {};branch={};rport\r\n",
        f.via_transport, f.via_sent_by, f.branch
    );
    out.push_str("Max-Forwards: 70\r\n");
    for route in routes {
        let _ = write!(out, "Route: {route}\r\n");
    }
    let _ = write!(out, "From: <{local_uri}>;tag={}\r\n", record.local_tag);
    let _ = write!(out, "To: <{remote_uri}>;tag={}\r\n", record.remote_tag);
    let _ = write!(out, "Call-ID: {}\r\n", record.call_id);
    let _ = write!(out, "CSeq: {} {}\r\n", record.local_cseq, f.method);
    out.push_str("Content-Length: 0\r\n\r\n");
    out.into_bytes()
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

    const SAMPLE_INVITE_TIMER: &str = concat!(
        "INVITE sip:alice@smiths.local SIP/2.0\r\n",
        "Via: SIP/2.0/TCP 10.0.0.1:5060;branch=z9hG4bK-inv-1\r\n",
        "Record-Route: <sip:p1.example;lr>, <sip:p2.example;lr>\r\n",
        "Record-Route: <sip:p3.example;lr>\r\n",
        "From: Bob <sip:bob@smiths.local>;tag=bob-1\r\n",
        "To: Alice <sip:alice@smiths.local>\r\n",
        "Call-ID: cid-timer@10.0.0.1\r\n",
        "CSeq: 7 INVITE\r\n",
        "Contact: <sip:bob@10.0.0.1:5060;transport=tcp>\r\n",
        "Supported: replaces, timer\r\n",
        "Session-Expires: 120;refresher=UAC\r\n",
        "Min-SE: 100\r\n",
        "Content-Length: 0\r\n\r\n",
    );

    fn record_with_routes(routes: &[&str], target: Option<&str>) -> DialogRecord {
        DialogRecord {
            call_id: "c@x".into(),
            local_tag: "lt".into(),
            remote_tag: "rt".into(),
            state: DialogState::Confirmed,
            peer_signal: "10.0.0.2:5060".parse().unwrap(),
            rendezvous: None,
            media: None,
            remote_media: None,
            pending_2xx: None,
            per_leg_codec: std::collections::BTreeMap::new(),
            ice: None,
            remote_target: target.map(str::to_owned),
            route_set: routes.iter().map(|r| (*r).to_owned()).collect(),
            local_uri: Some("sip:alice@smiths.local".into()),
            remote_uri: Some("sip:bob@smiths.local".into()),
            local_cseq: 3,
            remote_cseq: Some(9),
            transport: Some("TCP".into()),
            last_invite_branch: None,
            local_media: None,
            session_expires_secs: None,
        }
    }

    #[test]
    fn summary_extracts_branch_call_id_and_tags() {
        let raw = Bytes::copy_from_slice(SAMPLE_BYE.as_bytes());
        let s = summarize_request(&raw);
        assert_eq!(s.method, "BYE");
        assert_eq!(s.branch.as_deref(), Some("z9hG4bK-bye-1"));
        assert_eq!(s.call_id.as_deref(), Some("cid-xyz-42@10.0.0.1"));
        assert_eq!(s.from_tag.as_deref(), Some("bob-1"));
        assert_eq!(s.to_tag.as_deref(), Some("smiths-xyz"));
        assert_eq!(s.cseq, Some(2));
        assert!(!s.supports_timer);
        assert!(s.record_route.is_empty());
    }

    #[test]
    fn summary_extracts_routing_and_session_timer_headers() {
        let raw = Bytes::copy_from_slice(SAMPLE_INVITE_TIMER.as_bytes());
        let s = summarize_request(&raw);
        assert_eq!(s.cseq, Some(7));
        assert_eq!(
            s.record_route,
            vec![
                "<sip:p1.example;lr>".to_owned(),
                "<sip:p2.example;lr>".to_owned(),
                "<sip:p3.example;lr>".to_owned(),
            ]
        );
        assert_eq!(
            s.contact.as_deref(),
            Some("<sip:bob@10.0.0.1:5060;transport=tcp>")
        );
        assert!(s.supports_timer);
        assert_eq!(s.session_expires, Some(120));
        assert_eq!(s.session_refresher.as_deref(), Some("uac"));
        assert_eq!(s.min_se, Some(100));
    }

    #[test]
    fn session_expires_parses_without_refresher() {
        assert_eq!(parse_session_expires("1800"), (Some(1800), None));
        assert_eq!(
            parse_session_expires(" 90 ; refresher=uas"),
            (Some(90), Some("uas".into()))
        );
        assert_eq!(parse_session_expires("abc"), (None, None));
    }

    #[test]
    fn header_list_split_respects_angle_brackets() {
        assert_eq!(
            split_header_list("<sip:a;lr>, <sip:b?h=1,2>,c"),
            vec![
                "<sip:a;lr>".to_owned(),
                "<sip:b?h=1,2>".to_owned(),
                "c".to_owned()
            ]
        );
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

    #[test]
    fn via_branch_extracted_from_responses_and_requests() {
        let resp = Bytes::from_static(
            b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;rport;branch=z9hG4bK-r1\r\n\r\n",
        );
        assert_eq!(extract_via_branch(&resp).as_deref(), Some("z9hG4bK-r1"));
        let none = Bytes::from_static(b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 10.0.0.1\r\n\r\n");
        assert_eq!(extract_via_branch(&none), None);
        assert_eq!(response_status(&resp), Some(200));
    }

    #[test]
    fn route_target_without_route_set_uses_remote_target() {
        let rec = record_with_routes(&[], Some("sip:bob@10.0.0.2:5060"));
        let (ruri, routes) = dialog_route_target(&rec);
        assert_eq!(ruri, "sip:bob@10.0.0.2:5060");
        assert!(routes.is_empty());
    }

    #[test]
    fn route_target_falls_back_to_peer_signal() {
        let rec = record_with_routes(&[], None);
        let (ruri, _) = dialog_route_target(&rec);
        assert_eq!(ruri, "sip:10.0.0.2:5060");
    }

    #[test]
    fn route_target_loose_routing_keeps_target_and_copies_routes() {
        let rec = record_with_routes(
            &["<sip:p1.example;lr>", "<sip:p2.example;lr>"],
            Some("sip:bob@pc"),
        );
        let (ruri, routes) = dialog_route_target(&rec);
        assert_eq!(ruri, "sip:bob@pc");
        assert_eq!(
            routes,
            vec![
                "<sip:p1.example;lr>".to_owned(),
                "<sip:p2.example;lr>".to_owned()
            ]
        );
    }

    #[test]
    fn route_target_strict_router_becomes_request_uri() {
        let rec = record_with_routes(&["<sip:strict.example>", "<sip:p2;lr>"], Some("sip:bob@pc"));
        let (ruri, routes) = dialog_route_target(&rec);
        assert_eq!(ruri, "sip:strict.example");
        assert_eq!(
            routes,
            vec!["<sip:p2;lr>".to_owned(), "<sip:bob@pc>".to_owned()]
        );
    }

    #[test]
    fn in_dialog_request_mirrors_dialog_identity() {
        let rec = record_with_routes(&["<sip:p1.example;lr>"], Some("sip:bob@10.0.0.2:5060"));
        let bytes = build_in_dialog_request(&InDialogRequest {
            method: "BYE",
            record: &rec,
            via_sent_by: "10.0.0.9:5060".parse().unwrap(),
            via_transport: "TCP",
            branch: "z9hG4bK-b1",
        });
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(
            s.starts_with("BYE sip:bob@10.0.0.2:5060 SIP/2.0\r\n"),
            "{s}"
        );
        assert!(s.contains("Via: SIP/2.0/TCP 10.0.0.9:5060;branch=z9hG4bK-b1;rport\r\n"));
        assert!(s.contains("Route: <sip:p1.example;lr>\r\n"));
        assert!(s.contains("From: <sip:alice@smiths.local>;tag=lt\r\n"));
        assert!(s.contains("To: <sip:bob@smiths.local>;tag=rt\r\n"));
        assert!(s.contains("Call-ID: c@x\r\n"));
        assert!(s.contains("CSeq: 3 BYE\r\n"));
        assert!(s.ends_with("Content-Length: 0\r\n\r\n"));
    }

    #[test]
    fn dialog_fsm_projection_updates_record() {
        let mut rec = record_with_routes(&[], None);
        rec.state = DialogState::Early;
        assert_eq!(
            drive_dialog_fsm(&mut rec, DialogEvent::AckReceived),
            FsmOutcome::Continue
        );
        assert_eq!(rec.state, DialogState::Confirmed);
        assert_eq!(
            drive_dialog_fsm(&mut rec, DialogEvent::Cancelled),
            FsmOutcome::Illegal
        );
        assert_eq!(rec.state, DialogState::Confirmed);
        assert_eq!(
            drive_dialog_fsm(&mut rec, DialogEvent::ByeCompleted),
            FsmOutcome::Terminated
        );
    }

    #[test]
    fn session_timer_headers_shape() {
        let hdrs = session_timer_headers(Some(SessionTimerAgreement { interval_secs: 90 }));
        assert_eq!(
            hdrs,
            vec![
                ("Session-Expires", "90;refresher=uac".to_owned()),
                ("Require", "timer".to_owned())
            ]
        );
        assert!(session_timer_headers(None).is_empty());
    }

    #[test]
    fn warning_header_is_quoted_and_sanitized() {
        assert_eq!(
            format_warning(399, "no \"transcoder\""),
            "399 smiths-net \"no transcoder\""
        );
    }
}
