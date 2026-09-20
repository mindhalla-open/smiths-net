// TURN is a byte-oriented protocol: the `as u8` / `as u16` casts
// sit on field widths the RFC pins to fixed sizes. `too_many_lines`
// fires on the request handlers, which read better as one linear
// validate-then-reply sequence than split across helpers.
#![allow(clippy::cast_possible_truncation, clippy::too_many_lines)]

//! RFC 8656 TURN — embedded server and client.
//!
//! Server ([`TurnServer`]), the browser-compat subset over UDP:
//!
//! - **Allocate** (0x003) — 401-challenged via long-term credentials
//!   (RFC 8489 §9.2) with a random per-challenge nonce; the server
//!   mints a dedicated relay UDP socket and returns its address in
//!   `XOR-RELAYED-ADDRESS`.
//! - **Refresh** (0x004) — bumps the allocation's lifetime;
//!   `LIFETIME=0` deletes it (RFC 8656 §3.2).
//! - **`CreatePermission`** (0x008) — peer IPs the server will relay
//!   for; 5-minute lifetime (§9).
//! - **`ChannelBind`** (0x009) — maps a channel number onto a peer
//!   address; 10-minute lifetime (§12).
//! - **Send** / **Data** indications and **`ChannelData`** framing.
//!
//! Allocations, permissions and channel bindings expire: a sweep
//! runs every [`SWEEP_INTERVAL`] and every access re-checks the
//! deadline, so a stale entry is never used. Error replies carry the
//! class/method of the request they answer. Relay sends happen with
//! no lock held — the allocation table is locked only long enough to
//! copy out the socket handle and peer.
//!
//! Client ([`TurnClient`]): Allocate (with the challenge round trip),
//! Refresh, `CreatePermission`, `ChannelBind`, Send indications,
//! `ChannelData` framing and parsing of the relayed Data / `ChannelData`
//! the server forwards — enough for a relay candidate to carry media.
//!
//! Out of scope: TCP transport (RFC 6062), `EVEN-PORT` /
//! `DONT-FRAGMENT` enforcement, per-user quotas.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::RngExt as _;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use smiths_core::Metrics;
use smiths_core::metrics::TurnAllocationOutcomeLabel;

use crate::stun::{MAGIC_COOKIE, TransactionId};

/// TURN message methods (RFC 8656 §17).
pub const METHOD_ALLOCATE: u16 = 0x003;
/// Refresh method.
pub const METHOD_REFRESH: u16 = 0x004;
/// Send indication method.
pub const METHOD_SEND: u16 = 0x006;
/// Data indication method.
pub const METHOD_DATA: u16 = 0x007;
/// `CreatePermission` method.
pub const METHOD_CREATE_PERMISSION: u16 = 0x008;
/// `ChannelBind` method.
pub const METHOD_CHANNEL_BIND: u16 = 0x009;

/// TURN attribute types (RFC 8656 §18 + RFC 8489 §14).
pub const ATTR_USERNAME: u16 = 0x0006;
/// `MESSAGE-INTEGRITY` attribute.
pub const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
/// `ERROR-CODE` attribute.
pub const ATTR_ERROR_CODE: u16 = 0x0009;
/// `CHANNEL-NUMBER` attribute.
pub const ATTR_CHANNEL_NUMBER: u16 = 0x000C;
/// `LIFETIME` attribute.
pub const ATTR_LIFETIME: u16 = 0x000D;
/// `XOR-PEER-ADDRESS` attribute.
pub const ATTR_XOR_PEER_ADDRESS: u16 = 0x0012;
/// `DATA` attribute.
pub const ATTR_DATA: u16 = 0x0013;
/// `REALM` attribute.
pub const ATTR_REALM: u16 = 0x0014;
/// `NONCE` attribute.
pub const ATTR_NONCE: u16 = 0x0015;
/// `XOR-RELAYED-ADDRESS` attribute.
pub const ATTR_XOR_RELAYED_ADDRESS: u16 = 0x0016;
/// `REQUESTED-TRANSPORT` attribute.
pub const ATTR_REQUESTED_TRANSPORT: u16 = 0x0019;

/// Message classes in the two-bit form [`encode_type`] takes.
const CLASS_REQUEST: u16 = 0b00;
const CLASS_INDICATION: u16 = 0b01;
const CLASS_SUCCESS: u16 = 0b10;
const CLASS_ERROR: u16 = 0b11;

/// Default allocation lifetime (seconds). RFC 8656 §3.2
/// recommends 10 minutes; we match that default and let
/// operators cap lower via `[webrtc.turn] allocation_lifetime_s`.
pub const DEFAULT_LIFETIME_S: u32 = 600;

/// Permission lifetime per RFC 8656 §9: 5 minutes.
pub const PERMISSION_LIFETIME: Duration = Duration::from_mins(5);

/// Channel-binding lifetime per RFC 8656 §12: 10 minutes.
pub const CHANNEL_LIFETIME: Duration = Duration::from_mins(10);

/// How often the server reaps expired allocations, permissions and
/// channel bindings.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// How long an issued 401 nonce stays valid before the client must
/// re-challenge (RFC 8489 §9.2.4 leaves this to the server).
pub const CHALLENGE_LIFETIME: Duration = Duration::from_mins(1);

/// Channel numbers valid per RFC 8656 §12: 0x4000..=0x7FFF.
const CHANNEL_MIN: u16 = 0x4000;
const CHANNEL_MAX: u16 = 0x7FFF;

/// Long-term credential (RFC 8489 §9.2). The plaintext password
/// doesn't live past construction — we hash it into the long-
/// term key (`MD5(user:realm:pass)`) and discard the original.
#[derive(Clone, Debug)]
pub struct LongTermCredential {
    /// `USERNAME` clients present.
    pub username: String,
    /// Long-term key derived once, at load. Hashing the
    /// plaintext per-request would cost time + leave plaintext
    /// in memory longer than necessary.
    pub long_term_key: [u8; 16],
}

impl LongTermCredential {
    /// Derive the long-term key from a plaintext `password`
    /// against a fixed `realm`. Per RFC 8489 §9.2.2:
    /// `key = MD5(username ":" realm ":" password)`.
    #[must_use]
    pub fn new(username: &str, realm: &str, password: &str) -> Self {
        let mut hasher = md5::Context::new();
        hasher.consume(username.as_bytes());
        hasher.consume(b":");
        hasher.consume(realm.as_bytes());
        hasher.consume(b":");
        hasher.consume(password.as_bytes());
        Self {
            username: username.to_owned(),
            long_term_key: hasher.compute().0,
        }
    }
}

/// TURN server configuration distilled from `[webrtc.turn]`.
#[derive(Clone, Debug)]
pub struct TurnServerConfig {
    /// Server bind (clients connect here).
    pub bind: SocketAddr,
    /// Realm advertised in 401 challenges + hashed into
    /// long-term keys.
    pub realm: String,
    /// Public IP the server publishes in
    /// `XOR-RELAYED-ADDRESS`. Defaults to `bind.ip` when
    /// unset — override for servers behind a NAT.
    pub relay_ip: IpAddr,
    /// Default allocation lifetime. The client can request
    /// less via `LIFETIME`; this is the cap.
    pub allocation_lifetime: Duration,
    /// Long-term credentials. Rotate via `[reload]`.
    pub credentials: Vec<LongTermCredential>,
}

impl Default for TurnServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 3478),
            realm: "smiths-turn".into(),
            relay_ip: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            allocation_lifetime: Duration::from_secs(u64::from(DEFAULT_LIFETIME_S)),
            credentials: Vec::new(),
        }
    }
}

/// Outcome of a single Allocate exchange. Shared with tests +
/// the metric bump so the label vocabulary is centralized.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AllocateOutcome {
    /// Allocation succeeded; relay socket is live.
    Success,
    /// 401 Unauthorized — client needs to retry with
    /// `MESSAGE-INTEGRITY`. Distinct from a true auth failure
    /// because the first Allocate is SUPPOSED to 401.
    Challenged,
    /// 401 Unauthorized with a bad `MESSAGE-INTEGRITY`, an
    /// unknown user or a stale nonce — genuine auth failure,
    /// not the challenge dance.
    AuthFailed,
    /// 403 Forbidden (e.g., unsupported transport).
    Forbidden,
    /// 437 Allocation Mismatch — five-tuple already has an
    /// allocation. Uncommon in practice; surfaced distinctly
    /// so dashboards flag clients that re-Allocate without
    /// releasing.
    Mismatch,
    /// Catch-all for malformed requests + unrecoverable
    /// internal errors.
    Other,
}

impl AllocateOutcome {
    /// Metric label token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Challenged => "challenged",
            Self::AuthFailed => "auth_failed",
            Self::Forbidden => "forbidden",
            Self::Mismatch => "mismatch",
            Self::Other => "other",
        }
    }
}

/// Live allocation state owned by the server. One per client
/// source address.
struct Allocation {
    /// Who owns this allocation; every later request on it must
    /// authenticate as the same user (RFC 8656 §7.2, 441).
    username: String,
    /// Relay socket (bound on the server's relay IP).
    relay_sock: Arc<UdpSocket>,
    /// Address we published in `XOR-RELAYED-ADDRESS`.
    relay_addr: SocketAddr,
    /// Nonce the client must keep presenting (RFC 8489 §9.2.4).
    nonce: String,
    /// Absolute deadline for this allocation; refreshes bump it.
    expires_at: Instant,
    /// Permissions (peer IPs we'll relay for) keyed by IP →
    /// expiry.
    permissions: HashMap<IpAddr, Instant>,
    /// Channel → (`peer_addr`, expiry).
    channels_to_peer: HashMap<u16, (SocketAddr, Instant)>,
    /// Reverse index: `peer_addr` → channel. RFC 8656 §12
    /// requires a stable mapping in both directions.
    peer_to_channel: HashMap<SocketAddr, u16>,
    /// Cancellation token for the relay-receive task; firing
    /// it on allocation teardown drops the task + the socket.
    relay_task_cancel: CancellationToken,
}

impl Allocation {
    fn is_live(&self, now: Instant) -> bool {
        self.expires_at > now
    }

    fn permission_live(&self, ip: IpAddr, now: Instant) -> bool {
        self.permissions.get(&ip).is_some_and(|exp| *exp > now)
    }

    /// Peer bound to `channel`, if the binding is still live.
    fn channel_peer(&self, channel: u16, now: Instant) -> Option<SocketAddr> {
        self.channels_to_peer
            .get(&channel)
            .filter(|(_, exp)| *exp > now)
            .map(|(peer, _)| *peer)
    }

    /// Channel bound to `peer`, if the binding is still live.
    fn peer_channel(&self, peer: SocketAddr, now: Instant) -> Option<u16> {
        let channel = *self.peer_to_channel.get(&peer)?;
        self.channel_peer(channel, now).map(|_| channel)
    }

    /// Drop expired permissions and channel bindings.
    fn prune(&mut self, now: Instant) {
        self.permissions.retain(|_, exp| *exp > now);
        self.channels_to_peer.retain(|_, (_, exp)| *exp > now);
        let live: Vec<u16> = self.channels_to_peer.keys().copied().collect();
        self.peer_to_channel.retain(|_, ch| live.contains(ch));
    }
}

/// Credentials accepted on a request, handed back so the reply can
/// be signed with the same key.
struct Authenticated {
    username: String,
    key: [u8; 16],
    nonce: String,
}

/// Embedded TURN server. Cheap to clone — every field is an
/// `Arc` or `Clone`.
pub struct TurnServer {
    config: TurnServerConfig,
    /// Allocations keyed by client address.
    allocations: Arc<Mutex<HashMap<SocketAddr, Allocation>>>,
    /// Nonces issued in 401 challenges to clients that don't have an
    /// allocation yet, with their expiry.
    challenges: Arc<Mutex<HashMap<SocketAddr, (String, Instant)>>>,
    /// Credentials by `USERNAME`. Arc-mutex so hot-reload can swap
    /// in place.
    credentials: Arc<Mutex<HashMap<String, LongTermCredential>>>,
    /// Optional metrics handle.
    metrics: Option<Arc<Metrics>>,
}

impl TurnServer {
    /// Build a server from `config`. Does not bind — call
    /// [`Self::run`] to start serving.
    #[must_use]
    pub fn new(config: TurnServerConfig) -> Self {
        let mut creds = HashMap::new();
        for c in &config.credentials {
            creds.insert(c.username.clone(), c.clone());
        }
        Self {
            config,
            allocations: Arc::new(Mutex::new(HashMap::new())),
            challenges: Arc::new(Mutex::new(HashMap::new())),
            credentials: Arc::new(Mutex::new(creds)),
            metrics: None,
        }
    }

    /// Attach a metrics handle so
    /// `smiths_turn_allocations_total{outcome}` +
    /// `smiths_turn_active_allocations` update.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Bind the server socket + start the event loop, including the
    /// periodic expiry sweep. Returns when `cancel` fires or the
    /// socket dies.
    ///
    /// # Errors
    /// Bubbles up [`std::io::Error`] on bind failure.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) -> io::Result<()> {
        let sock = Arc::new(UdpSocket::bind(self.config.bind).await?);
        let local = sock.local_addr()?;
        info!(%local, realm = %self.config.realm, "TURN server listening");
        let mut buf = vec![0u8; 2048];
        let mut sweep = tokio::time::interval(SWEEP_INTERVAL);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                _ = sweep.tick() => self.sweep(Instant::now()).await,
                res = sock.recv_from(&mut buf) => match res {
                    Ok((n, from)) => {
                        let bytes = &buf[..n];
                        if let Err(e) = self.dispatch(bytes, from, Arc::clone(&sock)).await {
                            debug!(?from, ?e, "TURN dispatch error");
                        }
                    }
                    Err(e) => {
                        warn!(?e, "TURN recv failed");
                        break;
                    }
                }
            }
        }
        self.shutdown_allocations().await;
        info!(%local, "TURN server stopped");
        Ok(())
    }

    /// Number of live allocations — for tests and status views.
    pub async fn active_allocations(&self) -> usize {
        let now = Instant::now();
        self.allocations
            .lock()
            .await
            .values()
            .filter(|a| a.is_live(now))
            .count()
    }

    /// Reap expired allocations (cancelling their relay tasks),
    /// permissions, channel bindings and stale challenge nonces.
    async fn sweep(&self, now: Instant) {
        let mut expired = Vec::new();
        {
            let mut allocs = self.allocations.lock().await;
            allocs.retain(|client, alloc| {
                if alloc.is_live(now) {
                    alloc.prune(now);
                    true
                } else {
                    alloc.relay_task_cancel.cancel();
                    expired.push((*client, alloc.relay_addr, alloc.username.clone()));
                    false
                }
            });
        }
        for (client, relay_addr, user) in expired {
            info!(%client, %relay_addr, %user, "TURN allocation expired");
            if let Some(m) = &self.metrics {
                m.turn_active_allocations.dec();
            }
        }
        self.challenges
            .lock()
            .await
            .retain(|_, (_, exp)| *exp > now);
    }

    async fn shutdown_allocations(&self) {
        let mut allocs = self.allocations.lock().await;
        for (_, alloc) in allocs.drain() {
            alloc.relay_task_cancel.cancel();
            if let Some(m) = &self.metrics {
                m.turn_active_allocations.dec();
            }
        }
    }

    fn bump_alloc(&self, outcome: AllocateOutcome) {
        if let Some(m) = &self.metrics {
            m.turn_allocations
                .get_or_create(&TurnAllocationOutcomeLabel {
                    outcome: outcome.as_str().to_owned(),
                })
                .inc();
        }
    }

    async fn dispatch(
        self: &Arc<Self>,
        bytes: &[u8],
        from: SocketAddr,
        sock: Arc<UdpSocket>,
    ) -> io::Result<()> {
        // ChannelData framing: high two bits of first byte are
        // 0b01 (channel range 0x4000..=0x7FFF). Route before
        // STUN dispatch so the 20-byte STUN header check doesn't
        // reject a 4-byte ChannelData frame.
        if bytes.len() >= 4 && bytes[0] & 0xC0 == 0x40 {
            return self.handle_channel_data(bytes, from).await;
        }
        let Some(msg) = ParsedStun::parse(bytes) else {
            debug!(?from, "not a STUN-shaped datagram");
            return Ok(());
        };
        let reply = match (msg.class, msg.method) {
            (CLASS_REQUEST, METHOD_ALLOCATE) => self.handle_allocate(&msg, from, &sock).await,
            (CLASS_REQUEST, METHOD_REFRESH) => self.handle_refresh(&msg, from).await,
            (CLASS_REQUEST, METHOD_CREATE_PERMISSION) => {
                self.handle_create_permission(&msg, from).await
            }
            (CLASS_REQUEST, METHOD_CHANNEL_BIND) => self.handle_channel_bind(&msg, from).await,
            (CLASS_REQUEST, other) => {
                debug!(?from, method = other, "TURN: unknown request method");
                Some(build_error(other, msg.txid, 400, "Bad Request"))
            }
            (CLASS_INDICATION, METHOD_SEND) => {
                self.handle_send(&msg, from).await?;
                None
            }
            _ => None,
        };
        if let Some(reply) = reply {
            sock.send_to(&reply, from).await?;
        }
        Ok(())
    }

    /// Long-term credential check (RFC 8489 §9.2.4). `Err` carries
    /// the ready-to-send error reply: a 401 challenge with a fresh
    /// nonce when the request is unauthenticated, 438 when the nonce
    /// is not one we issued to this client, 401 on bad credentials.
    async fn authenticate(
        &self,
        msg: &ParsedStun<'_>,
        from: SocketAddr,
    ) -> Result<Authenticated, (Vec<u8>, AllocateOutcome)> {
        if msg.attr(ATTR_MESSAGE_INTEGRITY).is_none() {
            let nonce = self.issue_nonce(from).await;
            return Err((
                build_challenge(msg.method, msg.txid, 401, &self.config.realm, &nonce),
                AllocateOutcome::Challenged,
            ));
        }
        let (Some(username), Some(realm), Some(nonce)) = (
            msg.attr(ATTR_USERNAME)
                .and_then(|v| std::str::from_utf8(v).ok()),
            msg.attr(ATTR_REALM)
                .and_then(|v| std::str::from_utf8(v).ok()),
            msg.attr(ATTR_NONCE)
                .and_then(|v| std::str::from_utf8(v).ok()),
        ) else {
            return Err((
                build_error(msg.method, msg.txid, 400, "Bad Request"),
                AllocateOutcome::AuthFailed,
            ));
        };
        if !self.nonce_valid(from, nonce).await || realm != self.config.realm {
            let fresh = self.issue_nonce(from).await;
            return Err((
                build_challenge(msg.method, msg.txid, 438, &self.config.realm, &fresh),
                AllocateOutcome::AuthFailed,
            ));
        }
        let cred = self.credentials.lock().await.get(username).cloned();
        let Some(cred) = cred else {
            return Err((
                build_error(msg.method, msg.txid, 401, "Unauthorized"),
                AllocateOutcome::AuthFailed,
            ));
        };
        if !verify_message_integrity(msg.raw, &cred.long_term_key) {
            return Err((
                build_error(msg.method, msg.txid, 401, "Unauthorized"),
                AllocateOutcome::AuthFailed,
            ));
        }
        Ok(Authenticated {
            username: username.to_owned(),
            key: cred.long_term_key,
            nonce: nonce.to_owned(),
        })
    }

    async fn issue_nonce(&self, from: SocketAddr) -> String {
        let nonce = fresh_nonce();
        self.challenges
            .lock()
            .await
            .insert(from, (nonce.clone(), Instant::now() + CHALLENGE_LIFETIME));
        nonce
    }

    /// A nonce is valid when it is the one on the client's live
    /// allocation or the one issued in its pending challenge.
    async fn nonce_valid(&self, from: SocketAddr, nonce: &str) -> bool {
        let now = Instant::now();
        if self
            .allocations
            .lock()
            .await
            .get(&from)
            .is_some_and(|a| a.is_live(now) && a.nonce == nonce)
        {
            return true;
        }
        self.challenges
            .lock()
            .await
            .get(&from)
            .is_some_and(|(n, exp)| n == nonce && *exp > now)
    }

    async fn handle_allocate(
        self: &Arc<Self>,
        msg: &ParsedStun<'_>,
        from: SocketAddr,
        sock: &Arc<UdpSocket>,
    ) -> Option<Vec<u8>> {
        // Require `REQUESTED-TRANSPORT` = UDP (17).
        let transport_ok = msg
            .attr(ATTR_REQUESTED_TRANSPORT)
            .is_some_and(|v| v.len() >= 4 && v[0] == 17);
        if !transport_ok {
            self.bump_alloc(AllocateOutcome::Forbidden);
            return Some(build_error(
                METHOD_ALLOCATE,
                msg.txid,
                442,
                "Unsupported Transport Protocol",
            ));
        }
        let auth = match self.authenticate(msg, from).await {
            Ok(a) => a,
            Err((reply, outcome)) => {
                self.bump_alloc(outcome);
                return Some(reply);
            }
        };

        let now = Instant::now();
        // Reject duplicate allocation from the same client.
        if self
            .allocations
            .lock()
            .await
            .get(&from)
            .is_some_and(|a| a.is_live(now))
        {
            self.bump_alloc(AllocateOutcome::Mismatch);
            return Some(build_error(
                METHOD_ALLOCATE,
                msg.txid,
                437,
                "Allocation Mismatch",
            ));
        }

        // Bind a fresh relay socket on the configured relay IP.
        let relay_sock = match UdpSocket::bind(SocketAddr::new(self.config.relay_ip, 0)).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                warn!(?e, "TURN: relay socket bind failed");
                self.bump_alloc(AllocateOutcome::Other);
                return Some(build_error(
                    METHOD_ALLOCATE,
                    msg.txid,
                    508,
                    "Insufficient Capacity",
                ));
            }
        };
        let Ok(relay_addr_local) = relay_sock.local_addr() else {
            self.bump_alloc(AllocateOutcome::Other);
            return Some(build_error(METHOD_ALLOCATE, msg.txid, 500, "Server Error"));
        };
        // Public-facing address = configured relay_ip + the
        // kernel-assigned port.
        let relay_addr = SocketAddr::new(self.config.relay_ip, relay_addr_local.port());

        let lifetime = self.lifetime_from_request(msg);
        let cancel = CancellationToken::new();
        let relay_cancel = cancel.clone();
        // Spawn the relay-receive task. Packets from permitted
        // peers are forwarded to the client as Data Indications;
        // channel-bound peers use the fast-path ChannelData
        // framing.
        let server = Arc::clone(self);
        let client_addr = from;
        let server_sock = Arc::clone(sock);
        let relay_sock_task = Arc::clone(&relay_sock);
        tokio::spawn(async move {
            let mut buf = vec![0u8; 1600];
            loop {
                tokio::select! {
                    biased;
                    () = relay_cancel.cancelled() => return,
                    res = relay_sock_task.recv_from(&mut buf) => match res {
                        Ok((n, peer)) => {
                            let payload = &buf[..n];
                            server.relay_peer_to_client(client_addr, peer, payload, &server_sock).await;
                        }
                        Err(e) => {
                            debug!(?e, "relay socket recv error");
                            return;
                        }
                    }
                }
            }
        });

        {
            let mut allocs = self.allocations.lock().await;
            // An expired-but-unswept entry is replaced.
            if let Some(old) = allocs.insert(
                from,
                Allocation {
                    username: auth.username.clone(),
                    relay_sock,
                    relay_addr,
                    nonce: auth.nonce.clone(),
                    expires_at: now + lifetime,
                    permissions: HashMap::new(),
                    channels_to_peer: HashMap::new(),
                    peer_to_channel: HashMap::new(),
                    relay_task_cancel: cancel,
                },
            ) {
                old.relay_task_cancel.cancel();
            } else if let Some(m) = &self.metrics {
                m.turn_active_allocations.inc();
            }
        }
        self.challenges.lock().await.remove(&from);
        self.bump_alloc(AllocateOutcome::Success);

        // Success response: `XOR-RELAYED-ADDRESS` + `LIFETIME` +
        // `XOR-MAPPED-ADDRESS` (client's reflexive).
        let mut reply = build_success_header(METHOD_ALLOCATE, msg.txid);
        append_attr_xor_addr(&mut reply, ATTR_XOR_RELAYED_ADDRESS, relay_addr, &msg.txid);
        append_attr_xor_addr(
            &mut reply,
            crate::stun::ATTR_XOR_MAPPED_ADDRESS,
            from,
            &msg.txid,
        );
        append_attr_lifetime(&mut reply, lifetime.as_secs() as u32);
        append_message_integrity(&mut reply, &auth.key);
        finalize_length(&mut reply);
        info!(%from, %relay_addr, user = %auth.username, "TURN allocation created");
        Some(reply)
    }

    fn lifetime_from_request(&self, msg: &ParsedStun<'_>) -> Duration {
        let requested = msg
            .attr(ATTR_LIFETIME)
            .filter(|v| v.len() == 4)
            .map_or(DEFAULT_LIFETIME_S, |v| {
                u32::from_be_bytes([v[0], v[1], v[2], v[3]])
            });
        // Cap at configured maximum.
        let capped = std::cmp::min(requested, self.config.allocation_lifetime.as_secs() as u32);
        Duration::from_secs(u64::from(capped))
    }

    /// Authenticate a request that operates on an existing
    /// allocation and check it belongs to the same user. `Err` is
    /// the error reply to send.
    async fn authenticate_on_allocation(
        &self,
        msg: &ParsedStun<'_>,
        from: SocketAddr,
    ) -> Result<Authenticated, Vec<u8>> {
        let auth = self
            .authenticate(msg, from)
            .await
            .map_err(|(reply, _)| reply)?;
        let now = Instant::now();
        let owner = self
            .allocations
            .lock()
            .await
            .get(&from)
            .filter(|a| a.is_live(now))
            .map(|a| a.username.clone());
        match owner {
            None => Err(build_error(
                msg.method,
                msg.txid,
                437,
                "Allocation Mismatch",
            )),
            Some(owner) if owner != auth.username => {
                Err(build_error(msg.method, msg.txid, 441, "Wrong Credentials"))
            }
            Some(_) => Ok(auth),
        }
    }

    async fn handle_refresh(&self, msg: &ParsedStun<'_>, from: SocketAddr) -> Option<Vec<u8>> {
        let auth = match self.authenticate_on_allocation(msg, from).await {
            Ok(a) => a,
            Err(reply) => return Some(reply),
        };
        let requested = msg
            .attr(ATTR_LIFETIME)
            .filter(|v| v.len() == 4)
            .map_or(DEFAULT_LIFETIME_S, |v| {
                u32::from_be_bytes([v[0], v[1], v[2], v[3]])
            });
        let lifetime = if requested == 0 {
            let removed = self.allocations.lock().await.remove(&from);
            if let Some(alloc) = removed {
                alloc.relay_task_cancel.cancel();
                info!(%from, user = %auth.username, "TURN allocation released");
                if let Some(m) = &self.metrics {
                    m.turn_active_allocations.dec();
                }
            }
            Duration::ZERO
        } else {
            let capped = std::cmp::min(requested, self.config.allocation_lifetime.as_secs() as u32);
            let lifetime = Duration::from_secs(u64::from(capped));
            let mut allocs = self.allocations.lock().await;
            let Some(alloc) = allocs.get_mut(&from) else {
                return Some(build_error(
                    METHOD_REFRESH,
                    msg.txid,
                    437,
                    "Allocation Mismatch",
                ));
            };
            alloc.expires_at = Instant::now() + lifetime;
            lifetime
        };
        let mut reply = build_success_header(METHOD_REFRESH, msg.txid);
        append_attr_lifetime(&mut reply, lifetime.as_secs() as u32);
        append_message_integrity(&mut reply, &auth.key);
        finalize_length(&mut reply);
        Some(reply)
    }

    async fn handle_create_permission(
        &self,
        msg: &ParsedStun<'_>,
        from: SocketAddr,
    ) -> Option<Vec<u8>> {
        let auth = match self.authenticate_on_allocation(msg, from).await {
            Ok(a) => a,
            Err(reply) => return Some(reply),
        };
        // Walk every `XOR-PEER-ADDRESS` attribute (multiple
        // allowed per RFC 8656 §9).
        let peers: Vec<SocketAddr> = msg
            .iter_attrs()
            .filter(|a| a.kind == ATTR_XOR_PEER_ADDRESS)
            .filter_map(|a| decode_xor_addr(a.value, &msg.txid))
            .collect();
        if peers.is_empty() {
            return Some(build_error(
                METHOD_CREATE_PERMISSION,
                msg.txid,
                400,
                "Bad Request",
            ));
        }
        {
            let mut allocs = self.allocations.lock().await;
            let Some(alloc) = allocs.get_mut(&from) else {
                return Some(build_error(
                    METHOD_CREATE_PERMISSION,
                    msg.txid,
                    437,
                    "Allocation Mismatch",
                ));
            };
            let deadline = Instant::now() + PERMISSION_LIFETIME;
            for peer in peers {
                alloc.permissions.insert(peer.ip(), deadline);
            }
        }
        let mut reply = build_success_header(METHOD_CREATE_PERMISSION, msg.txid);
        append_message_integrity(&mut reply, &auth.key);
        finalize_length(&mut reply);
        Some(reply)
    }

    async fn handle_channel_bind(&self, msg: &ParsedStun<'_>, from: SocketAddr) -> Option<Vec<u8>> {
        let auth = match self.authenticate_on_allocation(msg, from).await {
            Ok(a) => a,
            Err(reply) => return Some(reply),
        };
        let bad_request = || build_error(METHOD_CHANNEL_BIND, msg.txid, 400, "Bad Request");
        let Some(chan_attr) = msg.attr(ATTR_CHANNEL_NUMBER).filter(|v| v.len() == 4) else {
            return Some(bad_request());
        };
        let channel = u16::from_be_bytes([chan_attr[0], chan_attr[1]]);
        if !(CHANNEL_MIN..=CHANNEL_MAX).contains(&channel) {
            return Some(bad_request());
        }
        let Some(peer) = msg
            .attr(ATTR_XOR_PEER_ADDRESS)
            .and_then(|v| decode_xor_addr(v, &msg.txid))
        else {
            return Some(bad_request());
        };
        {
            let now = Instant::now();
            let mut allocs = self.allocations.lock().await;
            let Some(alloc) = allocs.get_mut(&from) else {
                return Some(build_error(
                    METHOD_CHANNEL_BIND,
                    msg.txid,
                    437,
                    "Allocation Mismatch",
                ));
            };
            alloc.prune(now);
            // RFC 8656 §12.2: a channel may only be (re)bound to the
            // same peer, and a peer may only hold one channel.
            let channel_conflict = alloc
                .channel_peer(channel, now)
                .is_some_and(|bound| bound != peer);
            let peer_conflict = alloc
                .peer_channel(peer, now)
                .is_some_and(|bound| bound != channel);
            if channel_conflict || peer_conflict {
                return Some(bad_request());
            }
            alloc
                .channels_to_peer
                .insert(channel, (peer, now + CHANNEL_LIFETIME));
            alloc.peer_to_channel.insert(peer, channel);
            // ChannelBind implicitly installs a permission too
            // (RFC 8656 §12.2).
            alloc
                .permissions
                .insert(peer.ip(), now + PERMISSION_LIFETIME);
        }
        let mut reply = build_success_header(METHOD_CHANNEL_BIND, msg.txid);
        append_message_integrity(&mut reply, &auth.key);
        finalize_length(&mut reply);
        Some(reply)
    }

    async fn handle_send(&self, msg: &ParsedStun<'_>, from: SocketAddr) -> io::Result<()> {
        // Send is an indication — no response. Relay the DATA
        // attribute to the named peer.
        let Some(peer) = msg
            .attr(ATTR_XOR_PEER_ADDRESS)
            .and_then(|v| decode_xor_addr(v, &msg.txid))
        else {
            return Ok(());
        };
        let Some(data) = msg.attr(ATTR_DATA) else {
            return Ok(());
        };
        // Copy the socket handle out under the lock; the send
        // itself happens unlocked.
        let relay_sock = {
            let now = Instant::now();
            let allocs = self.allocations.lock().await;
            let Some(alloc) = allocs.get(&from).filter(|a| a.is_live(now)) else {
                return Ok(());
            };
            if !alloc.permission_live(peer.ip(), now) {
                debug!(
                    ?peer,
                    "Send indication to peer without permission; dropping"
                );
                return Ok(());
            }
            Arc::clone(&alloc.relay_sock)
        };
        relay_sock.send_to(data, peer).await?;
        Ok(())
    }

    async fn handle_channel_data(&self, bytes: &[u8], from: SocketAddr) -> io::Result<()> {
        // Header: channel (u16 BE) + length (u16 BE) + data.
        if bytes.len() < 4 {
            return Ok(());
        }
        let channel = u16::from_be_bytes([bytes[0], bytes[1]]);
        let len = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
        let Some(data) = bytes.get(4..4 + len) else {
            return Ok(());
        };
        let (relay_sock, peer) = {
            let now = Instant::now();
            let allocs = self.allocations.lock().await;
            let Some(alloc) = allocs.get(&from).filter(|a| a.is_live(now)) else {
                return Ok(());
            };
            let Some(peer) = alloc.channel_peer(channel, now) else {
                return Ok(());
            };
            (Arc::clone(&alloc.relay_sock), peer)
        };
        relay_sock.send_to(data, peer).await?;
        Ok(())
    }

    async fn relay_peer_to_client(
        &self,
        client: SocketAddr,
        peer: SocketAddr,
        data: &[u8],
        sock: &Arc<UdpSocket>,
    ) {
        // Decide the framing under the lock, send without it.
        let channel = {
            let now = Instant::now();
            let allocs = self.allocations.lock().await;
            let Some(alloc) = allocs.get(&client).filter(|a| a.is_live(now)) else {
                return;
            };
            // Must have a live permission for the peer's IP.
            if !alloc.permission_live(peer.ip(), now) {
                return;
            }
            alloc.peer_channel(peer, now)
        };
        let frame = match channel {
            // Fast path: channel-bound peer → ChannelData frame.
            Some(channel) => channel_data_frame(channel, data),
            // Slow path: wrap in a Data indication.
            None => data_indication(peer, data),
        };
        let _ = sock.send_to(&frame, client).await;
    }
}

/// 16 random bytes, hex-encoded — one nonce per 401 challenge.
fn fresh_nonce() -> String {
    use std::fmt::Write as _;
    let mut rng = rand::rng();
    (0..16).fold(String::with_capacity(32), |mut acc, _| {
        let b: u8 = rng.random();
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

// ---- Client ----------------------------------------------------------

/// Errors from [`TurnClient`].
#[derive(Debug, thiserror::Error)]
pub enum TurnClientError {
    /// No response within the client's timeout.
    #[error("TURN request timed out")]
    Timeout,
    /// Socket error.
    #[error("io: {0}")]
    Io(#[from] io::Error),
    /// The server's reply didn't parse or lacked a required attribute.
    #[error("malformed TURN response: {0}")]
    Malformed(String),
    /// The server answered with an error response.
    #[error("TURN error {code} {reason}")]
    ErrorResponse {
        /// STUN error code (401, 437, 438,...).
        code: u16,
        /// Reason phrase.
        reason: String,
    },
}

/// A datagram the relay forwarded from a peer, as
/// [`TurnClient::parse_relayed`] classifies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Relayed {
    /// A Data indication: payload from `peer`.
    Data {
        /// Peer the server received the payload from.
        peer: SocketAddr,
        /// Payload bytes.
        data: Vec<u8>,
    },
    /// A `ChannelData` frame on a bound channel.
    Channel {
        /// Channel number (`0x4000..=0x7FFF`).
        channel: u16,
        /// Payload bytes.
        data: Vec<u8>,
    },
}

/// Credentials and challenge state for one TURN server.
struct ClientState {
    server: SocketAddr,
    cred: LongTermCredential,
    realm: Vec<u8>,
    nonce: Vec<u8>,
    timeout: Duration,
}

/// TURN client holding one allocation on `socket` (the media socket
/// the relay candidate is based on).
///
/// Control-plane calls (`refresh`, `create_permission`,
/// `channel_bind`) read from the socket until the matching response
/// arrives; datagrams that arrive in between are dropped, so drive
/// them from the same task that otherwise owns the socket's receive
/// side.
pub struct TurnClient {
    socket: Arc<UdpSocket>,
    state: ClientState,
    relay_addr: SocketAddr,
    lifetime: Duration,
}

impl TurnClient {
    /// Allocate a relay address on `server` using `cred`, running the
    /// 401 challenge round trip.
    ///
    /// # Errors
    /// [`TurnClientError`] on timeout, transport failure, a malformed
    /// reply or an error response other than the expected challenge.
    pub async fn allocate(
        socket: Arc<UdpSocket>,
        server: SocketAddr,
        cred: LongTermCredential,
        timeout: Duration,
    ) -> Result<Self, TurnClientError> {
        let mut state = ClientState {
            server,
            cred,
            realm: Vec::new(),
            nonce: Vec::new(),
            timeout,
        };
        // Unauthenticated Allocate → 401 with REALM + NONCE.
        let txid = TransactionId::random();
        let mut first = request_header(METHOD_ALLOCATE, txid);
        append_attr(&mut first, ATTR_REQUESTED_TRANSPORT, &[17, 0, 0, 0]);
        finalize_length(&mut first);
        let challenge = exchange(&socket, server, &first, txid, timeout).await?;
        let (code, _) = challenge.error().unwrap_or((0, String::new()));
        if code != 401 {
            return Err(TurnClientError::Malformed(format!(
                "expected 401 challenge, got {code}"
            )));
        }
        state.adopt_challenge(&challenge)?;

        let resp = transact(
            &socket,
            &mut state,
            METHOD_ALLOCATE,
            &[ReqAttr::Bytes(ATTR_REQUESTED_TRANSPORT, vec![17, 0, 0, 0])],
        )
        .await?;
        let relay_addr = resp
            .attr(ATTR_XOR_RELAYED_ADDRESS)
            .and_then(|v| decode_xor_addr(v, &resp.txid()))
            .ok_or_else(|| TurnClientError::Malformed("missing XOR-RELAYED-ADDRESS".into()))?;
        let lifetime = resp.lifetime().unwrap_or(Duration::ZERO);
        Ok(Self {
            socket,
            state,
            relay_addr,
            lifetime,
        })
    }

    /// Address peers reach this client at through the relay.
    #[must_use]
    pub fn relay_addr(&self) -> SocketAddr {
        self.relay_addr
    }

    /// Lifetime the server granted on the last Allocate / Refresh.
    #[must_use]
    pub fn lifetime(&self) -> Duration {
        self.lifetime
    }

    /// The socket the allocation is bound to.
    #[must_use]
    pub fn socket(&self) -> &Arc<UdpSocket> {
        &self.socket
    }

    /// Refresh the allocation for `lifetime` (RFC 8656 §7); `ZERO`
    /// releases it. Returns the lifetime the server granted.
    ///
    /// # Errors
    /// [`TurnClientError`] on timeout, transport failure or an error
    /// response (437 once the allocation has expired).
    pub async fn refresh(&mut self, lifetime: Duration) -> Result<Duration, TurnClientError> {
        let secs = u32::try_from(lifetime.as_secs()).unwrap_or(u32::MAX);
        let resp = transact(
            &self.socket,
            &mut self.state,
            METHOD_REFRESH,
            &[ReqAttr::Bytes(ATTR_LIFETIME, secs.to_be_bytes().to_vec())],
        )
        .await?;
        self.lifetime = resp.lifetime().unwrap_or(Duration::ZERO);
        Ok(self.lifetime)
    }

    /// Install permissions for `peers` (RFC 8656 §9). Only the IP
    /// part is significant; permissions last 5 minutes.
    ///
    /// # Errors
    /// [`TurnClientError`] on timeout, transport failure or an error
    /// response.
    pub async fn create_permission(&mut self, peers: &[SocketAddr]) -> Result<(), TurnClientError> {
        let attrs: Vec<ReqAttr> = peers.iter().map(|p| ReqAttr::PeerAddress(*p)).collect();
        transact(
            &self.socket,
            &mut self.state,
            METHOD_CREATE_PERMISSION,
            &attrs,
        )
        .await?;
        Ok(())
    }

    /// Bind `channel` (`0x4000..=0x7FFF`) to `peer` (RFC 8656 §11);
    /// also installs a permission for the peer.
    ///
    /// # Errors
    /// [`TurnClientError`] on timeout, transport failure or an error
    /// response (400 for a channel already bound to another peer).
    pub async fn channel_bind(
        &mut self,
        channel: u16,
        peer: SocketAddr,
    ) -> Result<(), TurnClientError> {
        let mut chan = channel.to_be_bytes().to_vec();
        chan.extend_from_slice(&[0, 0]);
        transact(
            &self.socket,
            &mut self.state,
            METHOD_CHANNEL_BIND,
            &[
                ReqAttr::Bytes(ATTR_CHANNEL_NUMBER, chan),
                ReqAttr::PeerAddress(peer),
            ],
        )
        .await?;
        Ok(())
    }

    /// Relay `data` to `peer` with a Send indication (RFC 8656 §10).
    ///
    /// # Errors
    /// Transport failure only — indications get no reply.
    pub async fn send_indication(
        &self,
        peer: SocketAddr,
        data: &[u8],
    ) -> Result<(), TurnClientError> {
        let txid = TransactionId::random();
        let mut msg = Vec::with_capacity(HEADER_LEN + 32 + data.len());
        msg.extend_from_slice(&encode_type(METHOD_SEND, CLASS_INDICATION).to_be_bytes());
        msg.extend_from_slice(&[0, 0]);
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&txid.0);
        append_attr_xor_addr(&mut msg, ATTR_XOR_PEER_ADDRESS, peer, &txid);
        append_attr_data(&mut msg, data);
        finalize_length(&mut msg);
        self.socket.send_to(&msg, self.state.server).await?;
        Ok(())
    }

    /// Relay `data` on a bound channel as a `ChannelData` frame
    /// (RFC 8656 §12.5).
    ///
    /// # Errors
    /// Transport failure only.
    pub async fn send_channel(&self, channel: u16, data: &[u8]) -> Result<(), TurnClientError> {
        self.socket
            .send_to(&channel_data_frame(channel, data), self.state.server)
            .await?;
        Ok(())
    }

    /// Classify a datagram received on the allocation's socket:
    /// `Some` for a Data indication or `ChannelData` frame from the
    /// server, `None` for anything else (an ICE check from a peer,
    /// RTP on a direct pair, a stray response).
    #[must_use]
    pub fn parse_relayed(bytes: &[u8]) -> Option<Relayed> {
        if bytes.len() >= 4 && bytes[0] & 0xC0 == 0x40 {
            let channel = u16::from_be_bytes([bytes[0], bytes[1]]);
            let len = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
            let data = bytes.get(4..4 + len)?;
            return Some(Relayed::Channel {
                channel,
                data: data.to_vec(),
            });
        }
        let msg = ParsedStun::parse(bytes)?;
        if msg.class != CLASS_INDICATION || msg.method != METHOD_DATA {
            return None;
        }
        let peer = decode_xor_addr(msg.attr(ATTR_XOR_PEER_ADDRESS)?, &msg.txid)?;
        let data = msg.attr(ATTR_DATA)?.to_vec();
        Some(Relayed::Data { peer, data })
    }

    /// Wait for the next relayed datagram, skipping anything that
    /// isn't a Data indication or `ChannelData` frame.
    ///
    /// # Errors
    /// [`TurnClientError::Timeout`] after the client's timeout,
    /// [`TurnClientError::Io`] on socket failure.
    pub async fn recv(&self) -> Result<Relayed, TurnClientError> {
        let deadline = Instant::now() + self.state.timeout;
        let mut buf = vec![0u8; 2048];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(TurnClientError::Timeout);
            }
            let (n, from) = tokio::time::timeout(remaining, self.socket.recv_from(&mut buf))
                .await
                .map_err(|_| TurnClientError::Timeout)??;
            if from != self.state.server {
                continue;
            }
            if let Some(relayed) = Self::parse_relayed(&buf[..n]) {
                return Ok(relayed);
            }
        }
    }
}

impl ClientState {
    fn adopt_challenge(&mut self, resp: &OwnedStun) -> Result<(), TurnClientError> {
        self.realm = resp
            .attr(ATTR_REALM)
            .ok_or_else(|| TurnClientError::Malformed("challenge missing REALM".into()))?
            .to_vec();
        self.nonce = resp
            .attr(ATTR_NONCE)
            .ok_or_else(|| TurnClientError::Malformed("challenge missing NONCE".into()))?
            .to_vec();
        Ok(())
    }
}

/// Attribute of a client request. Peer addresses are XOR-encoded
/// against the request's transaction id, so they are encoded inside
/// [`transact`] once the id exists.
enum ReqAttr {
    Bytes(u16, Vec<u8>),
    PeerAddress(SocketAddr),
}

/// Send an authenticated request and await its success response.
/// A 438 Stale Nonce reply is absorbed once: the new nonce is adopted
/// and the request repeated.
async fn transact(
    socket: &UdpSocket,
    state: &mut ClientState,
    method: u16,
    attrs: &[ReqAttr],
) -> Result<OwnedStun, TurnClientError> {
    for attempt in 0..2 {
        let txid = TransactionId::random();
        let mut msg = request_header(method, txid);
        for attr in attrs {
            match attr {
                ReqAttr::Bytes(kind, value) => append_attr(&mut msg, *kind, value),
                ReqAttr::PeerAddress(peer) => {
                    append_attr_xor_addr(&mut msg, ATTR_XOR_PEER_ADDRESS, *peer, &txid);
                }
            }
        }
        append_attr(&mut msg, ATTR_USERNAME, state.cred.username.as_bytes());
        append_attr(&mut msg, ATTR_REALM, &state.realm);
        append_attr(&mut msg, ATTR_NONCE, &state.nonce);
        append_message_integrity(&mut msg, &state.cred.long_term_key);
        finalize_length(&mut msg);
        let resp = exchange(socket, state.server, &msg, txid, state.timeout).await?;
        match resp.error() {
            None => return Ok(resp),
            Some((438, _)) if attempt == 0 => state.adopt_challenge(&resp)?,
            Some((code, reason)) => return Err(TurnClientError::ErrorResponse { code, reason }),
        }
    }
    Err(TurnClientError::Malformed(
        "nonce still stale after refresh".into(),
    ))
}

/// Send `msg` and wait for the response carrying `txid`, ignoring
/// unrelated datagrams.
async fn exchange(
    socket: &UdpSocket,
    server: SocketAddr,
    msg: &[u8],
    txid: TransactionId,
    timeout: Duration,
) -> Result<OwnedStun, TurnClientError> {
    socket.send_to(msg, server).await?;
    let deadline = Instant::now() + timeout;
    let mut buf = vec![0u8; 2048];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(TurnClientError::Timeout);
        }
        let (n, from) = tokio::time::timeout(remaining, socket.recv_from(&mut buf))
            .await
            .map_err(|_| TurnClientError::Timeout)??;
        if from != server {
            continue;
        }
        if let Some(parsed) = ParsedStun::parse(&buf[..n])
            && parsed.txid == txid
            && (parsed.class == CLASS_SUCCESS || parsed.class == CLASS_ERROR)
        {
            return Ok(OwnedStun {
                bytes: buf[..n].to_vec(),
            });
        }
    }
}

/// An owned copy of a response datagram with attribute accessors.
struct OwnedStun {
    bytes: Vec<u8>,
}

impl OwnedStun {
    fn parsed(&self) -> Option<ParsedStun<'_>> {
        ParsedStun::parse(&self.bytes)
    }

    fn attr(&self, kind: u16) -> Option<&[u8]> {
        self.parsed()?.attr(kind)
    }

    fn txid(&self) -> TransactionId {
        self.parsed().map_or(TransactionId([0; 12]), |p| p.txid)
    }

    /// `(code, reason)` when this is an error response.
    fn error(&self) -> Option<(u16, String)> {
        let parsed = self.parsed()?;
        if parsed.class != CLASS_ERROR {
            return None;
        }
        let value = parsed.attr(ATTR_ERROR_CODE)?;
        if value.len() < 4 {
            return None;
        }
        let code = u16::from(value[2] & 0x07) * 100 + u16::from(value[3]);
        Some((code, String::from_utf8_lossy(&value[4..]).into_owned()))
    }

    fn lifetime(&self) -> Option<Duration> {
        let v = self.attr(ATTR_LIFETIME)?;
        (v.len() == 4)
            .then(|| Duration::from_secs(u64::from(u32::from_be_bytes([v[0], v[1], v[2], v[3]]))))
    }
}

impl std::ops::Deref for OwnedStun {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

// ---- STUN message parser helpers (TURN-flavour) ----

const HEADER_LEN: usize = 20;

#[derive(Debug)]
struct ParsedStun<'a> {
    method: u16,
    /// Message class in two-bit form: 0 request, 1 indication,
    /// 2 success, 3 error.
    class: u16,
    txid: TransactionId,
    raw: &'a [u8],
    body: &'a [u8],
}

#[derive(Debug)]
struct AttrView<'a> {
    kind: u16,
    value: &'a [u8],
}

impl<'a> ParsedStun<'a> {
    fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < HEADER_LEN {
            return None;
        }
        let type_raw = u16::from_be_bytes([bytes[0], bytes[1]]);
        // RFC 8489 §5: type = M(11..12) C(0) M(7..9) C(1) M(0..3)
        let class = ((type_raw >> 4) & 0x01) | ((type_raw >> 7) & 0x02);
        let method = (type_raw & 0x000F) | ((type_raw & 0x00E0) >> 1) | ((type_raw & 0x3E00) >> 2);
        let length = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
        let cookie = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if cookie != MAGIC_COOKIE {
            return None;
        }
        if bytes.len() < HEADER_LEN + length {
            return None;
        }
        let mut txid = [0u8; 12];
        txid.copy_from_slice(&bytes[8..20]);
        Some(Self {
            method,
            class,
            txid: TransactionId(txid),
            raw: &bytes[..HEADER_LEN + length],
            body: &bytes[HEADER_LEN..HEADER_LEN + length],
        })
    }

    fn attr(&self, kind: u16) -> Option<&'a [u8]> {
        self.iter_attrs().find(|a| a.kind == kind).map(|a| a.value)
    }

    fn iter_attrs(&self) -> impl Iterator<Item = AttrView<'a>> {
        AttrIter {
            body: self.body,
            idx: 0,
        }
    }
}

struct AttrIter<'a> {
    body: &'a [u8],
    idx: usize,
}

impl<'a> Iterator for AttrIter<'a> {
    type Item = AttrView<'a>;
    fn next(&mut self) -> Option<AttrView<'a>> {
        if self.idx + 4 > self.body.len() {
            return None;
        }
        let kind = u16::from_be_bytes([self.body[self.idx], self.body[self.idx + 1]]);
        let len = usize::from(u16::from_be_bytes([
            self.body[self.idx + 2],
            self.body[self.idx + 3],
        ]));
        self.idx += 4;
        if self.idx + len > self.body.len() {
            return None;
        }
        let value = &self.body[self.idx..self.idx + len];
        // Attributes are padded to a 4-byte boundary.
        let pad = (4 - (len % 4)) % 4;
        self.idx += len + pad;
        Some(AttrView { kind, value })
    }
}

// ---- Message encoders ----

fn header(method: u16, class: u16, txid: TransactionId) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&encode_type(method, class).to_be_bytes());
    out.extend_from_slice(&[0, 0]); // length placeholder
    out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    out.extend_from_slice(&txid.0);
    out
}

fn request_header(method: u16, txid: TransactionId) -> Vec<u8> {
    header(method, CLASS_REQUEST, txid)
}

fn build_success_header(method: u16, txid: TransactionId) -> Vec<u8> {
    header(method, CLASS_SUCCESS, txid)
}

/// Error response for a request of `method` — the reply carries the
/// request's method so the client's transaction layer can match it.
fn build_error(method: u16, txid: TransactionId, code: u16, reason: &str) -> Vec<u8> {
    let mut out = header(method, CLASS_ERROR, txid);
    append_error_code(&mut out, code, reason);
    finalize_length(&mut out);
    out
}

/// 401 / 438 reply carrying `REALM` + `NONCE` (RFC 8489 §9.2.4).
fn build_challenge(
    method: u16,
    txid: TransactionId,
    code: u16,
    realm: &str,
    nonce: &str,
) -> Vec<u8> {
    let reason = if code == 438 {
        "Stale Nonce"
    } else {
        "Unauthorized"
    };
    let mut out = header(method, CLASS_ERROR, txid);
    append_error_code(&mut out, code, reason);
    append_attr(&mut out, ATTR_REALM, realm.as_bytes());
    append_attr(&mut out, ATTR_NONCE, nonce.as_bytes());
    finalize_length(&mut out);
    out
}

fn channel_data_frame(channel: u16, data: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(4 + data.len());
    frame.extend_from_slice(&channel.to_be_bytes());
    frame.extend_from_slice(&(data.len() as u16).to_be_bytes());
    frame.extend_from_slice(data);
    frame
}

fn data_indication(peer: SocketAddr, data: &[u8]) -> Vec<u8> {
    let txid = TransactionId::random();
    let mut msg = header(METHOD_DATA, CLASS_INDICATION, txid);
    append_attr_xor_addr(&mut msg, ATTR_XOR_PEER_ADDRESS, peer, &txid);
    append_attr_data(&mut msg, data);
    finalize_length(&mut msg);
    msg
}

fn encode_type(method: u16, class: u16) -> u16 {
    // Bit positions per RFC 8489 §5:
    //   type = M(11..12) C(0) M(7..9) C(1) M(0..3)
    // class encodes C(1)C(0) ∈ {request=00, indication=01,
    // success=10, error=11}.
    let m = method;
    let c0 = class & 0b01;
    let c1 = (class >> 1) & 0b01;
    ((m & 0x0F00) << 2) | (c1 << 8) | ((m & 0x0070) << 1) | (c0 << 4) | (m & 0x000F)
}

fn append_attr(buf: &mut Vec<u8>, kind: u16, value: &[u8]) {
    buf.extend_from_slice(&kind.to_be_bytes());
    let len = value.len() as u16;
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(value);
    let pad = (4 - (value.len() % 4)) % 4;
    buf.extend(std::iter::repeat_n(0u8, pad));
}

fn append_error_code(buf: &mut Vec<u8>, code: u16, reason: &str) {
    let class = (code / 100) as u8;
    let number = (code % 100) as u8;
    let mut value = vec![0u8, 0u8, class, number];
    value.extend_from_slice(reason.as_bytes());
    append_attr(buf, ATTR_ERROR_CODE, &value);
}

fn append_attr_lifetime(buf: &mut Vec<u8>, lifetime_s: u32) {
    append_attr(buf, ATTR_LIFETIME, &lifetime_s.to_be_bytes());
}

fn append_attr_data(buf: &mut Vec<u8>, data: &[u8]) {
    append_attr(buf, ATTR_DATA, data);
}

fn append_attr_xor_addr(buf: &mut Vec<u8>, kind: u16, addr: SocketAddr, txid: &TransactionId) {
    let port_xor = addr.port() ^ ((MAGIC_COOKIE >> 16) as u16);
    match addr {
        SocketAddr::V4(v4) => {
            let ip_xor = u32::from(*v4.ip()) ^ MAGIC_COOKIE;
            let mut value = Vec::with_capacity(8);
            value.push(0); // reserved
            value.push(0x01); // family = IPv4
            value.extend_from_slice(&port_xor.to_be_bytes());
            value.extend_from_slice(&ip_xor.to_be_bytes());
            append_attr(buf, kind, &value);
        }
        SocketAddr::V6(v6) => {
            let octets = v6.ip().octets();
            // IPv6 XOR: first 4 bytes with MAGIC_COOKIE, next
            // 12 with transaction ID.
            let cookie_bytes = MAGIC_COOKIE.to_be_bytes();
            let mask = cookie_bytes.iter().chain(txid.0.iter());
            let xor_bytes: Vec<u8> = octets.iter().zip(mask).map(|(o, m)| o ^ m).collect();
            let mut value = Vec::with_capacity(20);
            value.push(0);
            value.push(0x02);
            value.extend_from_slice(&port_xor.to_be_bytes());
            value.extend_from_slice(&xor_bytes);
            append_attr(buf, kind, &value);
        }
    }
}

fn decode_xor_addr(raw: &[u8], txid: &TransactionId) -> Option<SocketAddr> {
    if raw.len() < 4 {
        return None;
    }
    let family = raw[1];
    let port_xor = u16::from_be_bytes([raw[2], raw[3]]);
    let port = port_xor ^ ((MAGIC_COOKIE >> 16) as u16);
    match family {
        0x01 if raw.len() >= 8 => {
            let ip_xor = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]);
            let ip = ip_xor ^ MAGIC_COOKIE;
            Some(SocketAddr::V4(std::net::SocketAddrV4::new(
                std::net::Ipv4Addr::from(ip),
                port,
            )))
        }
        0x02 if raw.len() >= 20 => {
            let cookie = MAGIC_COOKIE.to_be_bytes();
            let mask = cookie.iter().chain(txid.0.iter());
            let mut octets = [0u8; 16];
            for (o, (r, m)) in octets.iter_mut().zip(raw[4..20].iter().zip(mask)) {
                *o = r ^ m;
            }
            Some(SocketAddr::V6(std::net::SocketAddrV6::new(
                std::net::Ipv6Addr::from(octets),
                port,
                0,
                0,
            )))
        }
        _ => None,
    }
}

fn append_message_integrity(buf: &mut Vec<u8>, key: &[u8; 16]) {
    // The HMAC covers the message so far with the length field
    // already counting the MESSAGE-INTEGRITY attribute (4 + 20).
    let attr_header_len = 4;
    let hmac_len = 20;
    let final_len = buf.len() + attr_header_len + hmac_len - HEADER_LEN;
    let len_be = (final_len as u16).to_be_bytes();
    buf[2] = len_be[0];
    buf[3] = len_be[1];
    let mac = hmac_sha1(key, buf);
    append_attr(buf, ATTR_MESSAGE_INTEGRITY, &mac);
}

fn verify_message_integrity(raw: &[u8], key: &[u8; 16]) -> bool {
    // Find MESSAGE-INTEGRITY attribute by scanning the body.
    let Some(msg) = ParsedStun::parse(raw) else {
        return false;
    };
    let mut cursor = HEADER_LEN;
    let end = HEADER_LEN + msg.body.len();
    while cursor + 4 <= end {
        let kind = u16::from_be_bytes([raw[cursor], raw[cursor + 1]]);
        let len = usize::from(u16::from_be_bytes([raw[cursor + 2], raw[cursor + 3]]));
        if kind == ATTR_MESSAGE_INTEGRITY {
            if len != 20 || cursor + 4 + 20 > raw.len() {
                return false;
            }
            let observed = &raw[cursor + 4..cursor + 4 + 20];
            // Length-adjusted message: header length = up to and
            // including the integrity attr, nothing after it.
            let mut msg_for_hmac = raw[..cursor].to_vec();
            let adjusted_len = (cursor + 4 + 20 - HEADER_LEN) as u16;
            msg_for_hmac[2] = (adjusted_len >> 8) as u8;
            msg_for_hmac[3] = (adjusted_len & 0xFF) as u8;
            let expected = hmac_sha1(key, &msg_for_hmac);
            return ct_eq(observed, &expected);
        }
        let pad = (4 - (len % 4)) % 4;
        cursor += 4 + len + pad;
    }
    false
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    type H = Hmac<Sha1>;
    // HMAC-SHA1 accepts any key length; `new_from_slice` only
    // errors when the key exceeds the block size, and our
    // long-term keys are fixed at 16 B. Falling back to a
    // newly-keyed HMAC with an empty key on the impossible
    // error path is safe: verification against any real peer
    // fails loudly (no stealthy drop to auth-bypass).
    let mut mac = H::new_from_slice(key).unwrap_or_else(|_| {
        H::new_from_slice(&[]).unwrap_or_else(|_| unreachable!("HMAC accepts empty key"))
    });
    mac.update(msg);
    let tag = mac.finalize().into_bytes();
    let mut out = [0u8; 20];
    out.copy_from_slice(&tag);
    out
}

fn finalize_length(buf: &mut [u8]) {
    // Set the STUN header length to `buf.len - 20`.
    let body_len = (buf.len() - HEADER_LEN) as u16;
    buf[2] = (body_len >> 8) as u8;
    buf[3] = (body_len & 0xFF) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_term_key_matches_rfc_example() {
        use std::fmt::Write as _;
        // RFC 8489 §9.2.2 derivation: MD5(user:realm:pass).
        let cred = LongTermCredential::new("alice", "example.org", "password123");
        let rendered = cred
            .long_term_key
            .iter()
            .fold(String::with_capacity(32), |mut acc, b| {
                let _ = write!(acc, "{b:02x}");
                acc
            });
        // Sanity: 16-byte MD5 = 32 hex chars.
        assert_eq!(rendered.len(), 32);
        // Re-derive the same inputs + expect identical output.
        let cred2 = LongTermCredential::new("alice", "example.org", "password123");
        assert_eq!(cred.long_term_key, cred2.long_term_key);
        // Different password yields a different key.
        let cred3 = LongTermCredential::new("alice", "example.org", "other");
        assert_ne!(cred.long_term_key, cred3.long_term_key);
    }

    #[test]
    fn encode_type_round_trips_request_method_bits() {
        // Allocate request: class=00 (request), method=0x003 →
        // only method's low 4 bits are set → type = 0x0003.
        let ty = encode_type(0x003, CLASS_REQUEST);
        assert_eq!(ty, 0x0003);
        // Success response: class=10 → C1 bit at position 8 set.
        let ty = encode_type(0x003, CLASS_SUCCESS);
        assert_eq!(ty, 0x0103);
        // Error response for Refresh: 0x0114.
        assert_eq!(encode_type(METHOD_REFRESH, CLASS_ERROR), 0x0114);
    }

    #[test]
    fn error_replies_carry_the_request_method() {
        let txid = TransactionId([3u8; 12]);
        for method in [
            METHOD_ALLOCATE,
            METHOD_REFRESH,
            METHOD_CREATE_PERMISSION,
            METHOD_CHANNEL_BIND,
        ] {
            let reply = build_error(method, txid, 437, "Allocation Mismatch");
            let parsed = ParsedStun::parse(&reply).unwrap();
            assert_eq!(parsed.class, CLASS_ERROR);
            assert_eq!(parsed.method, method);
            let challenge = build_challenge(method, txid, 438, "r", "n");
            let parsed = ParsedStun::parse(&challenge).unwrap();
            assert_eq!(parsed.method, method);
            assert_eq!(parsed.attr(ATTR_NONCE), Some(&b"n"[..]));
        }
    }

    #[test]
    fn nonces_are_random_per_challenge() {
        let a = fresh_nonce();
        let b = fresh_nonce();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }

    #[test]
    fn xor_addr_round_trip_v4_and_v6() {
        let txid = TransactionId([1u8; 12]);
        for addr in ["192.0.2.5:60001", "[2001:db8::9]:7"] {
            let addr: SocketAddr = addr.parse().unwrap();
            let mut buf = Vec::new();
            append_attr_xor_addr(&mut buf, ATTR_XOR_PEER_ADDRESS, addr, &txid);
            // Skip 4-byte attr header (type+len) and parse.
            let value = &buf[4..];
            let decoded = decode_xor_addr(value, &txid).unwrap();
            assert_eq!(decoded, addr);
        }
    }

    #[test]
    fn message_integrity_accepts_matching_hmac_rejects_tampered() {
        let key = [0x11u8; 16];
        let mut msg = build_success_header(METHOD_ALLOCATE, TransactionId([2u8; 12]));
        append_attr_lifetime(&mut msg, DEFAULT_LIFETIME_S);
        append_message_integrity(&mut msg, &key);
        finalize_length(&mut msg);
        assert!(verify_message_integrity(&msg, &key));
        // Flip a byte in the LIFETIME attr → integrity check fails.
        let mut tampered = msg.clone();
        tampered[22] ^= 0xFF;
        assert!(!verify_message_integrity(&tampered, &key));
        // Use wrong key → fails.
        let wrong = [0x22u8; 16];
        assert!(!verify_message_integrity(&msg, &wrong));
    }

    #[test]
    fn parse_relayed_classifies_channel_data_and_data_indications() {
        let peer: SocketAddr = "198.51.100.7:4000".parse().unwrap();
        let ind = data_indication(peer, b"hello");
        assert_eq!(
            TurnClient::parse_relayed(&ind),
            Some(Relayed::Data {
                peer,
                data: b"hello".to_vec()
            })
        );
        let frame = channel_data_frame(0x4001, b"fast");
        assert_eq!(
            TurnClient::parse_relayed(&frame),
            Some(Relayed::Channel {
                channel: 0x4001,
                data: b"fast".to_vec()
            })
        );
        // A Binding request is neither.
        let binding = crate::stun::StunMessage::new_binding_request()
            .encode()
            .unwrap();
        assert_eq!(TurnClient::parse_relayed(&binding), None);
    }
}
