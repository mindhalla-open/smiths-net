//! ICE agent (RFC 8445): candidate pairing, authenticated
//! connectivity checks, peer-reflexive discovery, nomination,
//! role-conflict resolution and keepalives for one media component.
//!
//! The agent is sans-IO: it never reads or writes a socket itself. The
//! caller owns the media sockets (the fabric's endpoint sockets, which
//! also carry RTP), demultiplexes inbound datagrams with
//! [`crate::stun::is_stun`] and hands STUN ones to
//! [`IceAgent::handle_datagram`]; [`IceAgent::tick`] returns the
//! datagrams to send.
//!
//! ## Intended call sequence
//!
//! 1. Offer/answer: `NegotiationOutcome::Accepted { ice: Some(params),.. }`
//!    carries the local + remote `ice-ufrag` / `ice-pwd`, the role and
//!    the tie-breaker (`smiths_sdp::Negotiator` with
//!    `with_ice_enabled(true)`).
//! 2. Candidates: local ones from [`crate::CandidateGatherer`]
//!    (`gather` for host, `gather_all` for host + srflx + relay) on
//!    the sockets the media fabric allocated; remote ones from the
//!    offer's `a=candidate` lines and, later, trickle frames via
//!    [`IceAgent::add_remote_candidate`].
//! 3. `IceAgent::new(params, &local, &remote, sockets)` where `sockets`
//!    maps each local candidate's base address to its `UdpSocket`.
//! 4. Drive it: every ≤ 50 ms call [`IceAgent::tick`] and send each
//!    [`Outgoing`]; for every STUN datagram received on one of the
//!    sockets call [`IceAgent::handle_datagram`] and send the reply it
//!    returns. Stop when [`IceAgent::state`] is
//!    [`IceState::Completed`] (media goes to
//!    [`IceAgent::selected_remote`]) or [`IceState::Failed`] (tear the
//!    call down; [`IceAgent::failure_reason`] says why). After
//!    completion keep ticking about once a second so the RFC 8445 §11
//!    keepalive indication goes out every 15 s. A controlled agent
//!    whose checks succeed but that never receives a nomination stays
//!    `Checking`; the driver owns that deadline.
//!
//! The WebRTC signaling handler in `crates/smiths-cli/src/webrtc.rs`
//! is the intended driver; it already parses trickle candidates.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use smiths_core::Metrics;
use smiths_core::metrics::{IceBindingCheckLabel, IceCandidateTypeLabel};
use smiths_core::sdp::{IceParams, IceRole};
use smiths_sdp::IceCandidate;
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

use crate::stun::{
    ERR_BAD_REQUEST, ERR_ROLE_CONFLICT, ERR_UNAUTHENTICATED, StunClass, StunMessage, TransactionId,
    verify_fingerprint, verify_message_integrity,
};

/// Pacing between new connectivity checks (RFC 8445 §14.2, `Ta`).
pub const CHECK_PACING: Duration = Duration::from_millis(50);
/// Initial retransmission timeout for a check (RFC 8445 §14.3, `RTO`).
pub const RETRANSMIT_TIMEOUT: Duration = Duration::from_millis(500);
/// Retransmissions before a check is declared lost (RFC 8489 §6.2.1,
/// `Rc`).
pub const MAX_RETRANSMITS: u32 = 7;
/// Keepalive interval on the selected pair (RFC 8445 §11, `Tr`).
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Type preference of a peer-reflexive candidate (RFC 8445 §5.1.2.2).
const PRFLX_TYPE_PREFERENCE: u32 = 110;

/// Connectivity check state for a candidate pair (RFC 8445 §6.1.2.6).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PairState {
    /// Check has not been sent yet.
    Frozen,
    /// Waiting for its turn in the check list.
    Waiting,
    /// Binding Request has been sent, awaiting response.
    InProgress,
    /// Valid response received.
    Succeeded,
    /// Check failed (timeout, error response, or source mismatch).
    Failed,
}

/// Overall agent state (RFC 8445 §6.1.2.1 check-list state, collapsed
/// to what a driver acts on).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IceState {
    /// Checks are running or waiting for the peer's nomination.
    Checking,
    /// A pair was nominated; media flows on
    /// [`IceAgent::selected_remote`].
    Completed,
    /// Every pair failed; see [`IceAgent::failure_reason`].
    Failed,
}

/// A candidate pair (RFC 8445 §6.1.2.2).
#[derive(Clone, Debug)]
pub struct IcePair {
    /// Local candidate.
    pub local: IceCandidate,
    /// Remote candidate.
    pub remote: IceCandidate,
    /// Pair priority per RFC 8445 §6.1.2.3.
    pub priority: u64,
    /// Current state of the connectivity check.
    pub state: PairState,
    /// Whether this pair was nominated (by us when controlling, by
    /// the peer's `USE-CANDIDATE` when controlled).
    pub nominated: bool,
    /// Address the peer reported in `XOR-MAPPED-ADDRESS` on the
    /// successful check.
    pub mapped_address: Option<SocketAddr>,
    /// Peer sent `USE-CANDIDATE` for this pair; honored once the
    /// pair's own check succeeds (RFC 8445 §7.3.1.5).
    nominate_requested: bool,
    /// The check was rejected with 401 — repeating it with the same
    /// credentials cannot succeed.
    auth_failed: bool,
}

impl IcePair {
    fn remote_addr(&self) -> SocketAddr {
        SocketAddr::new(self.remote.address, self.remote.port)
    }

    fn local_addr(&self) -> SocketAddr {
        SocketAddr::new(self.local.address, self.local.port)
    }
}

/// A datagram the driver must send on `socket` to `destination`.
pub struct Outgoing {
    /// Encoded STUN message.
    pub bytes: Vec<u8>,
    /// Peer address.
    pub destination: SocketAddr,
    /// Local socket to send from (the pair's local base).
    pub socket: Arc<UdpSocket>,
}

struct PendingCheck {
    pair_idx: usize,
    transaction_id: TransactionId,
    bytes: Vec<u8>,
    destination: SocketAddr,
    socket: Arc<UdpSocket>,
    last_sent: Instant,
    retransmits: u32,
    nominating: bool,
}

/// ICE agent state machine (RFC 8445 §6–§8) for one component.
pub struct IceAgent {
    params: IceParams,
    role: IceRole,
    local_candidates: Vec<IceCandidate>,
    remote_candidates: Vec<IceCandidate>,
    pairs: Vec<IcePair>,
    /// Triggered checks (`pair index`, `nominate`) — sent before
    /// ordinary checks (RFC 8445 §6.1.4.1).
    triggered: VecDeque<(usize, bool)>,
    pending: Vec<PendingCheck>,
    sockets: HashMap<SocketAddr, Arc<UdpSocket>>,
    state: IceState,
    selected: Option<usize>,
    nomination_in_flight: bool,
    last_check_sent: Option<Instant>,
    last_keepalive: Option<Instant>,
    keepalive_interval: Duration,
    failure: Option<String>,
    metrics: Option<Arc<Metrics>>,
}

impl IceAgent {
    /// Create an agent from negotiated `params`, the local and remote
    /// candidate lists and the local sockets keyed by candidate base
    /// address. Local candidates without a socket are ignored.
    #[must_use]
    pub fn new(
        params: IceParams,
        local_candidates: &[IceCandidate],
        remote_candidates: &[IceCandidate],
        sockets: HashMap<SocketAddr, Arc<UdpSocket>>,
    ) -> Self {
        let role = params.role;
        let mut agent = Self {
            params,
            role,
            local_candidates: local_candidates
                .iter()
                .filter(|c| sockets.contains_key(&SocketAddr::new(c.address, c.port)))
                .cloned()
                .collect(),
            remote_candidates: Vec::new(),
            pairs: Vec::new(),
            triggered: VecDeque::new(),
            pending: Vec::new(),
            sockets,
            state: IceState::Checking,
            selected: None,
            nomination_in_flight: false,
            last_check_sent: None,
            last_keepalive: None,
            keepalive_interval: KEEPALIVE_INTERVAL,
            failure: None,
            metrics: None,
        };
        for remote in remote_candidates {
            agent.add_remote_candidate(remote);
        }
        agent
    }

    /// Attach the engine's metrics handle so
    /// `smiths_ice_binding_checks_total{outcome}` and
    /// `smiths_ice_candidates_gathered_total{type="prflx"}` update.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Override the keepalive interval (default
    /// [`KEEPALIVE_INTERVAL`]).
    #[must_use]
    pub fn with_keepalive_interval(mut self, interval: Duration) -> Self {
        self.keepalive_interval = interval;
        self
    }

    /// Current agent state.
    #[must_use]
    pub fn state(&self) -> IceState {
        self.state
    }

    /// Why the agent failed, once [`Self::state`] is
    /// [`IceState::Failed`].
    #[must_use]
    pub fn failure_reason(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Current role — may differ from the negotiated one after a
    /// role conflict (RFC 8445 §7.3.1.1).
    #[must_use]
    pub fn role(&self) -> IceRole {
        self.role
    }

    /// Every candidate pair with its check state.
    #[must_use]
    pub fn pairs(&self) -> &[IcePair] {
        &self.pairs
    }

    /// Local candidates, including peer-reflexive ones learned from
    /// `XOR-MAPPED-ADDRESS`.
    #[must_use]
    pub fn local_candidates(&self) -> &[IceCandidate] {
        &self.local_candidates
    }

    /// Remote candidates, including peer-reflexive ones learned from
    /// inbound checks.
    #[must_use]
    pub fn remote_candidates(&self) -> &[IceCandidate] {
        &self.remote_candidates
    }

    /// The nominated pair once the agent is [`IceState::Completed`].
    #[must_use]
    pub fn selected_pair(&self) -> Option<&IcePair> {
        self.selected.and_then(|i| self.pairs.get(i))
    }

    /// Remote address media should be sent to, once completed.
    #[must_use]
    pub fn selected_remote(&self) -> Option<SocketAddr> {
        self.selected_pair().map(IcePair::remote_addr)
    }

    /// Local socket media should be sent from, once completed.
    #[must_use]
    pub fn selected_socket(&self) -> Option<Arc<UdpSocket>> {
        self.selected_pair()
            .and_then(|p| self.sockets.get(&p.local_addr()).cloned())
    }

    /// Add a remote candidate learned after construction (trickle
    /// ICE). Forms pairs with every local candidate of the same
    /// component and address family; a `Failed` agent goes back to
    /// `Checking`.
    pub fn add_remote_candidate(&mut self, remote: &IceCandidate) {
        let remote_addr = SocketAddr::new(remote.address, remote.port);
        if self.remote_candidates.iter().any(|c| {
            SocketAddr::new(c.address, c.port) == remote_addr && c.component == remote.component
        }) {
            return;
        }
        self.remote_candidates.push(remote.clone());
        for local in self.local_candidates.clone() {
            self.form_pair(&local, remote);
        }
        if self.state == IceState::Failed {
            self.state = IceState::Checking;
            self.failure = None;
        }
    }

    fn form_pair(&mut self, local: &IceCandidate, remote: &IceCandidate) -> Option<usize> {
        if local.component != remote.component
            || local.address.is_ipv4() != remote.address.is_ipv4()
        {
            return None;
        }
        let local_addr = SocketAddr::new(local.address, local.port);
        let remote_addr = SocketAddr::new(remote.address, remote.port);
        if let Some(idx) = self.find_pair(local_addr, remote_addr) {
            return Some(idx);
        }
        let priority = self.pair_priority(local.priority, remote.priority);
        self.pairs.push(IcePair {
            local: local.clone(),
            remote: remote.clone(),
            priority,
            state: PairState::Waiting,
            nominated: false,
            mapped_address: None,
            nominate_requested: false,
            auth_failed: false,
        });
        Some(self.pairs.len() - 1)
    }

    /// RFC 8445 §6.1.2.3: `2^32 * MIN(G,D) + 2 * MAX(G,D) + (G > D)`
    /// with `G` the controlling agent's candidate priority.
    fn pair_priority(&self, local: u32, remote: u32) -> u64 {
        let (g, d) = match self.role {
            IceRole::Controlling => (local, remote),
            IceRole::Controlled => (remote, local),
        };
        (u64::from(g.min(d)) << 32) + (u64::from(g.max(d)) << 1) + u64::from(g > d)
    }

    fn find_pair(&self, local: SocketAddr, remote: SocketAddr) -> Option<usize> {
        self.pairs
            .iter()
            .position(|p| p.local_addr() == local && p.remote_addr() == remote)
    }

    fn switch_role(&mut self, role: IceRole) {
        warn!(from = ?self.role, to = ?role, "ICE role conflict: switching role");
        self.role = role;
        for i in 0..self.pairs.len() {
            let (l, r) = (self.pairs[i].local.priority, self.pairs[i].remote.priority);
            self.pairs[i].priority = self.pair_priority(l, r);
        }
    }

    // ---- Driver entry points ------------------------------------------

    /// Advance timers: retransmit outstanding checks, start the next
    /// triggered / ordinary check, send keepalives once completed.
    /// Returns the datagrams the driver must send.
    pub fn tick(&mut self) -> Vec<Outgoing> {
        self.tick_at(Instant::now())
    }

    /// [`Self::tick`] with an explicit clock, for deterministic tests.
    pub fn tick_at(&mut self, now: Instant) -> Vec<Outgoing> {
        let mut out = Vec::new();
        match self.state {
            IceState::Failed => return out,
            IceState::Completed => {
                if let Some(keepalive) = self.keepalive(now) {
                    out.push(keepalive);
                }
                return out;
            }
            IceState::Checking => {}
        }

        self.retransmit(now, &mut out);

        let paced = self
            .last_check_sent
            .is_none_or(|t| now.duration_since(t) >= CHECK_PACING);
        if paced {
            let next = self
                .next_triggered()
                .or_else(|| self.next_ordinary().map(|i| (i, false)));
            if let Some((idx, nominate)) = next
                && let Some(o) = self.send_check(idx, nominate, now)
            {
                out.push(o);
            }
        }
        self.update_failure();
        out
    }

    /// Feed a STUN datagram received on the socket bound to
    /// `local_addr` from `from`. Returns the reply to send back to
    /// `from` on that same socket, if any.
    pub fn handle_datagram(
        &mut self,
        bytes: &[u8],
        from: SocketAddr,
        local_addr: SocketAddr,
    ) -> Option<Vec<u8>> {
        self.handle_datagram_at(bytes, from, local_addr, Instant::now())
    }

    /// [`Self::handle_datagram`] with an explicit clock.
    pub fn handle_datagram_at(
        &mut self,
        bytes: &[u8],
        from: SocketAddr,
        local_addr: SocketAddr,
        now: Instant,
    ) -> Option<Vec<u8>> {
        let msg = match StunMessage::decode(bytes) {
            Ok(m) => m,
            Err(e) => {
                debug!(%from, ?e, "ICE: dropping undecodable STUN datagram");
                return None;
            }
        };
        match msg.class {
            StunClass::Request => self.handle_request(&msg, bytes, from, local_addr),
            StunClass::SuccessResponse => {
                self.handle_success(&msg, bytes, from, local_addr);
                None
            }
            StunClass::ErrorResponse => {
                self.handle_error(&msg, bytes, from, now);
                None
            }
            // Keepalive indications need no reply (RFC 8445 §11).
            StunClass::Indication => None,
        }
    }

    // ---- Outbound checks ----------------------------------------------

    fn next_triggered(&mut self) -> Option<(usize, bool)> {
        while let Some((idx, nominate)) = self.triggered.pop_front() {
            if matches!(
                self.pairs[idx].state,
                PairState::Waiting | PairState::Frozen | PairState::Succeeded
            ) {
                return Some((idx, nominate));
            }
        }
        None
    }

    fn next_ordinary(&self) -> Option<usize> {
        self.pairs
            .iter()
            .enumerate()
            .filter(|(_, p)| p.state == PairState::Waiting)
            .max_by_key(|(_, p)| p.priority)
            .map(|(i, _)| i)
    }

    fn send_check(&mut self, idx: usize, nominate: bool, now: Instant) -> Option<Outgoing> {
        let pair = &self.pairs[idx];
        let socket = self.sockets.get(&pair.local_addr())?.clone();
        let destination = pair.remote_addr();

        let mut msg = StunMessage::new_binding_request();
        msg.username = Some(format!(
            "{}:{}",
            self.params.remote_ufrag, self.params.local_ufrag
        ));
        msg.priority = Some(prflx_priority(&pair.local));
        match self.role {
            IceRole::Controlling => {
                msg.ice_controlling = Some(self.params.tie_breaker);
                msg.use_candidate = nominate;
            }
            IceRole::Controlled => msg.ice_controlled = Some(self.params.tie_breaker),
        }
        let bytes = match msg.encode_with(Some(self.params.remote_pwd.as_bytes()), true) {
            Ok(b) => b,
            Err(e) => {
                warn!(?e, "ICE: failed to encode connectivity check");
                return None;
            }
        };
        if self.pairs[idx].state != PairState::Succeeded {
            self.pairs[idx].state = PairState::InProgress;
        }
        self.pending.push(PendingCheck {
            pair_idx: idx,
            transaction_id: msg.transaction_id,
            bytes: bytes.clone(),
            destination,
            socket: Arc::clone(&socket),
            last_sent: now,
            retransmits: 0,
            nominating: nominate,
        });
        self.last_check_sent = Some(now);
        debug!(%destination, nominate, "ICE: connectivity check sent");
        Some(Outgoing {
            bytes,
            destination,
            socket,
        })
    }

    fn retransmit(&mut self, now: Instant, out: &mut Vec<Outgoing>) {
        let mut timed_out = Vec::new();
        for (i, check) in self.pending.iter_mut().enumerate() {
            let backoff = RETRANSMIT_TIMEOUT * (1u32 << check.retransmits.min(16));
            if now.duration_since(check.last_sent) < backoff {
                continue;
            }
            if check.retransmits < MAX_RETRANSMITS {
                check.retransmits += 1;
                check.last_sent = now;
                out.push(Outgoing {
                    bytes: check.bytes.clone(),
                    destination: check.destination,
                    socket: Arc::clone(&check.socket),
                });
            } else {
                timed_out.push(i);
            }
        }
        for i in timed_out.into_iter().rev() {
            let check = self.pending.remove(i);
            let pair = &mut self.pairs[check.pair_idx];
            if pair.state != PairState::Succeeded {
                pair.state = PairState::Failed;
            }
            debug!(remote = %check.destination, "ICE: connectivity check timed out");
            self.bump_check("timeout");
            if check.nominating {
                self.nomination_in_flight = false;
            }
        }
    }

    fn keepalive(&mut self, now: Instant) -> Option<Outgoing> {
        let due = self
            .last_keepalive
            .is_none_or(|t| now.duration_since(t) >= self.keepalive_interval);
        if !due {
            return None;
        }
        let pair = self.selected_pair()?;
        let socket = self.sockets.get(&pair.local_addr())?.clone();
        let destination = pair.remote_addr();
        // RFC 8445 §11: a Binding Indication with no credentials —
        // only the FINGERPRINT.
        let bytes = StunMessage::new_binding_indication()
            .encode_with(None, true)
            .ok()?;
        self.last_keepalive = Some(now);
        Some(Outgoing {
            bytes,
            destination,
            socket,
        })
    }

    // ---- Inbound handling ---------------------------------------------

    fn handle_request(
        &mut self,
        msg: &StunMessage,
        raw: &[u8],
        from: SocketAddr,
        local_addr: SocketAddr,
    ) -> Option<Vec<u8>> {
        // RFC 8445 §7.2.2: ICE checks always carry FINGERPRINT; a
        // request without a valid one is not addressed to us.
        if !msg.has_fingerprint || !verify_fingerprint(raw) {
            debug!(%from, "ICE: request without valid FINGERPRINT dropped");
            return None;
        }
        let key: Vec<u8> = self.params.local_pwd.as_bytes().to_vec();
        let key = key.as_slice();
        let (Some(username), true) = (&msg.username, msg.has_message_integrity) else {
            return Self::error_reply(msg, ERR_BAD_REQUEST, "Bad Request", None);
        };
        let expected = format!("{}:{}", self.params.local_ufrag, self.params.remote_ufrag);
        if *username != expected || !verify_message_integrity(raw, key) {
            debug!(%from, "ICE: request failed short-term credential check");
            return Self::error_reply(msg, ERR_UNAUTHENTICATED, "Unauthenticated", None);
        }

        // RFC 8445 §7.3.1.1: role conflict — the agent with the larger
        // tie-breaker keeps its role and answers 487.
        match (self.role, msg.ice_controlling, msg.ice_controlled) {
            (IceRole::Controlling, Some(theirs), _) => {
                if self.params.tie_breaker >= theirs {
                    return Self::error_reply(msg, ERR_ROLE_CONFLICT, "Role Conflict", Some(key));
                }
                self.switch_role(IceRole::Controlled);
            }
            (IceRole::Controlled, _, Some(theirs)) => {
                if self.params.tie_breaker >= theirs {
                    self.switch_role(IceRole::Controlling);
                } else {
                    return Self::error_reply(msg, ERR_ROLE_CONFLICT, "Role Conflict", Some(key));
                }
            }
            _ => {}
        }

        // RFC 8445 §7.3.1.3: an unknown source is a peer-reflexive
        // candidate; §7.3.1.4: schedule a triggered check on the pair.
        let component = self
            .local_candidates
            .iter()
            .find(|c| SocketAddr::new(c.address, c.port) == local_addr)
            .map_or(1, |c| c.component);
        if !self
            .remote_candidates
            .iter()
            .any(|c| SocketAddr::new(c.address, c.port) == from)
        {
            let prflx = IceCandidate {
                foundation: format!("prflx{}", self.remote_candidates.len()),
                component,
                transport: "UDP".into(),
                priority: msg.priority.unwrap_or(0),
                address: from.ip(),
                port: from.port(),
                candidate_type: "prflx".into(),
                related_address: None,
                related_port: None,
                raw_params: Vec::new(),
            };
            info!(%from, "ICE: learned peer-reflexive remote candidate");
            self.add_remote_candidate(&prflx);
        }
        if let Some(idx) = self.find_pair(local_addr, from) {
            if msg.use_candidate && self.role == IceRole::Controlled {
                self.pairs[idx].nominate_requested = true;
            }
            match self.pairs[idx].state {
                PairState::Succeeded => {
                    if self.pairs[idx].nominate_requested {
                        self.nominate(idx);
                    }
                }
                PairState::InProgress => {}
                PairState::Waiting | PairState::Frozen => self.triggered.push_back((idx, false)),
                PairState::Failed => {
                    if !self.pairs[idx].auth_failed {
                        self.pairs[idx].state = PairState::Waiting;
                        self.triggered.push_back((idx, false));
                    }
                }
            }
            if self.state == IceState::Failed {
                self.state = IceState::Checking;
                self.failure = None;
            }
        }

        let resp = StunMessage::new_binding_response(msg, from);
        resp.encode_with(Some(key), true).ok()
    }

    fn error_reply(
        request: &StunMessage,
        code: u16,
        reason: &str,
        key: Option<&[u8]>,
    ) -> Option<Vec<u8>> {
        StunMessage::new_binding_error(request, code, reason)
            .encode_with(key, true)
            .ok()
    }

    fn handle_success(
        &mut self,
        msg: &StunMessage,
        raw: &[u8],
        from: SocketAddr,
        local_addr: SocketAddr,
    ) {
        let Some(pos) = self
            .pending
            .iter()
            .position(|c| c.transaction_id == msg.transaction_id)
        else {
            return;
        };
        // RFC 8445 §7.2.5.1: responses carry MESSAGE-INTEGRITY keyed
        // with the peer's password; anything else is discarded and
        // the request keeps retransmitting.
        if !verify_fingerprint(raw)
            || !verify_message_integrity(raw, self.params.remote_pwd.as_bytes())
        {
            debug!(%from, "ICE: success response failed integrity check; ignored");
            return;
        }
        let check = self.pending.remove(pos);
        let idx = check.pair_idx;
        if from != self.pairs[idx].remote_addr() || local_addr != self.pairs[idx].local_addr() {
            // RFC 8445 §7.2.5.2.1: symmetric transport check failed.
            debug!(%from, "ICE: response from unexpected address; pair failed");
            self.pairs[idx].state = PairState::Failed;
            self.bump_check("mismatch");
            if check.nominating {
                self.nomination_in_flight = false;
            }
            self.update_failure();
            return;
        }
        if let Some(mapped) = msg.xor_mapped_address {
            self.pairs[idx].mapped_address = Some(mapped);
            self.learn_local_prflx(mapped, idx);
        }
        self.pairs[idx].state = PairState::Succeeded;
        self.bump_check("success");
        debug!(remote = %from, nominating = check.nominating, "ICE: connectivity check succeeded");

        if check.nominating || self.pairs[idx].nominate_requested {
            self.nominate(idx);
        } else if self.role == IceRole::Controlling
            && !self.nomination_in_flight
            && self.selected.is_none()
        {
            // Regular nomination (RFC 8445 §8.1.1): nominate the first
            // pair that validated with a second, flagged check.
            self.nomination_in_flight = true;
            self.triggered.push_front((idx, true));
        }
    }

    /// RFC 8445 §7.2.5.3.1: a mapped address that isn't a known local
    /// candidate is a peer-reflexive local candidate, based on the
    /// socket the check went out on.
    fn learn_local_prflx(&mut self, mapped: SocketAddr, pair_idx: usize) {
        if self
            .local_candidates
            .iter()
            .any(|c| SocketAddr::new(c.address, c.port) == mapped)
        {
            return;
        }
        let base = self.pairs[pair_idx].local.clone();
        let prflx = IceCandidate {
            foundation: format!("prflx{}", self.local_candidates.len()),
            component: base.component,
            transport: "UDP".into(),
            priority: prflx_priority(&base),
            address: mapped.ip(),
            port: mapped.port(),
            candidate_type: "prflx".into(),
            related_address: Some(base.address),
            related_port: Some(base.port),
            raw_params: Vec::new(),
        };
        info!(%mapped, base = %self.pairs[pair_idx].local_addr(), "ICE: learned peer-reflexive local candidate");
        self.local_candidates.push(prflx);
        if let Some(m) = &self.metrics {
            m.ice_candidates_gathered
                .get_or_create(&IceCandidateTypeLabel { ty: "prflx".into() })
                .inc();
        }
    }

    fn handle_error(&mut self, msg: &StunMessage, raw: &[u8], from: SocketAddr, now: Instant) {
        let Some(pos) = self
            .pending
            .iter()
            .position(|c| c.transaction_id == msg.transaction_id)
        else {
            return;
        };
        let code = msg.error_code.as_ref().map_or(0, |e| e.code);
        if code == ERR_ROLE_CONFLICT {
            // RFC 8445 §7.2.5.1: a 487 must be integrity-protected;
            // switch role and repeat the check.
            if !verify_message_integrity(raw, self.params.remote_pwd.as_bytes()) {
                debug!(%from, "ICE: unauthenticated 487 ignored");
                return;
            }
            let check = self.pending.remove(pos);
            let flipped = match self.role {
                IceRole::Controlling => IceRole::Controlled,
                IceRole::Controlled => IceRole::Controlling,
            };
            self.switch_role(flipped);
            self.pairs[check.pair_idx].state = PairState::Waiting;
            self.nomination_in_flight = false;
            self.triggered.push_back((check.pair_idx, false));
            // The repeat is due immediately, not after another pacing
            // interval.
            self.last_check_sent = None;
            let _ = now;
            return;
        }
        let check = self.pending.remove(pos);
        let pair = &mut self.pairs[check.pair_idx];
        pair.state = PairState::Failed;
        pair.auth_failed = code == ERR_UNAUTHENTICATED;
        if check.nominating {
            self.nomination_in_flight = false;
        }
        warn!(%from, code, reason = ?msg.error_code.as_ref().map(|e| &e.reason), "ICE: connectivity check rejected");
        self.bump_check("error");
        self.update_failure();
    }

    fn nominate(&mut self, idx: usize) {
        if self.state == IceState::Completed {
            return;
        }
        self.pairs[idx].nominated = true;
        self.selected = Some(idx);
        self.state = IceState::Completed;
        self.last_keepalive = Some(Instant::now());
        self.pending.clear();
        self.triggered.clear();
        info!(
            local = %self.pairs[idx].local_addr(),
            remote = %self.pairs[idx].remote_addr(),
            role = ?self.role,
            "ICE: pair nominated, checks complete"
        );
    }

    /// RFC 8445 §8.1.2: once nothing is in flight, nothing is waiting
    /// and no pair succeeded, the check list — and this single-list
    /// agent — has failed.
    fn update_failure(&mut self) {
        if self.state != IceState::Checking
            || !self.pending.is_empty()
            || !self.triggered.is_empty()
        {
            return;
        }
        let live = self.pairs.iter().any(|p| {
            matches!(
                p.state,
                PairState::Waiting
                    | PairState::Frozen
                    | PairState::InProgress
                    | PairState::Succeeded
            )
        });
        if live {
            return;
        }
        let auth = self.pairs.iter().filter(|p| p.auth_failed).count();
        let reason = if self.pairs.is_empty() {
            "no candidate pairs could be formed".to_owned()
        } else if auth == self.pairs.len() {
            format!("all {auth} candidate pairs rejected the credentials (401)")
        } else {
            format!(
                "all {} candidate pairs failed ({auth} credential rejections)",
                self.pairs.len()
            )
        };
        warn!(%reason, "ICE: connectivity checks failed");
        self.failure = Some(reason);
        self.state = IceState::Failed;
    }

    fn bump_check(&self, outcome: &str) {
        if let Some(m) = &self.metrics {
            m.ice_binding_checks
                .get_or_create(&IceBindingCheckLabel {
                    outcome: outcome.to_owned(),
                })
                .inc();
        }
    }
}

/// Priority the peer should assign if it learns this candidate as
/// peer-reflexive (RFC 8445 §7.1.1): same local preference and
/// component, type preference 110.
fn prflx_priority(local: &IceCandidate) -> u32 {
    let local_pref = (local.priority >> 8) & 0xFFFF;
    (PRFLX_TYPE_PREFERENCE << 24) + (local_pref << 8) + (256 - u32::from(local.component))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::gather_host_candidates;
    use crate::stun::{StunClass, verify_fingerprint, verify_message_integrity};

    const A_UFRAG: &str = "aaaa";
    const A_PWD: &str = "aaaaaaaaaaaaaaaaaaaaaa";
    const B_UFRAG: &str = "bbbb";
    const B_PWD: &str = "bbbbbbbbbbbbbbbbbbbbbb";

    fn params(role: IceRole, tie_breaker: u64) -> IceParams {
        IceParams {
            local_ufrag: A_UFRAG.into(),
            local_pwd: A_PWD.into(),
            remote_ufrag: B_UFRAG.into(),
            remote_pwd: B_PWD.into(),
            role,
            tie_breaker,
        }
    }

    /// Agent "A" bound on a loopback socket, with one remote host
    /// candidate for "B" at `remote`.
    async fn agent(role: IceRole, tie_breaker: u64, remote: SocketAddr) -> (IceAgent, SocketAddr) {
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let local_addr = sock.local_addr().unwrap();
        let local = gather_host_candidates(&[local_addr], 1);
        let remote_cands = gather_host_candidates(&[remote], 1);
        let mut sockets = HashMap::new();
        sockets.insert(local_addr, sock);
        (
            IceAgent::new(params(role, tie_breaker), &local, &remote_cands, sockets),
            local_addr,
        )
    }

    /// A request as "B" would send it to "A": username
    /// `A_UFRAG:B_UFRAG`, signed with A's password.
    fn request_from_b(role_attr: (Option<u64>, Option<u64>), use_candidate: bool) -> StunMessage {
        let mut req = StunMessage::new_binding_request();
        req.username = Some(format!("{A_UFRAG}:{B_UFRAG}"));
        req.priority = Some(1_000);
        req.ice_controlling = role_attr.0;
        req.ice_controlled = role_attr.1;
        req.use_candidate = use_candidate;
        req
    }

    #[tokio::test]
    async fn tick_sends_authenticated_check_with_role_attribute() {
        let remote: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        let (mut a, _) = agent(IceRole::Controlling, 42, remote).await;
        let out = a.tick();
        assert_eq!(out.len(), 1, "one check for the single pair");
        assert_eq!(out[0].destination, remote);
        let msg = StunMessage::decode(&out[0].bytes).unwrap();
        assert_eq!(msg.class, StunClass::Request);
        assert_eq!(msg.username.as_deref(), Some("bbbb:aaaa"));
        assert_eq!(msg.ice_controlling, Some(42));
        assert!(
            !msg.use_candidate,
            "regular nomination: first check is not flagged"
        );
        assert!(msg.priority.is_some());
        assert!(verify_fingerprint(&out[0].bytes));
        assert!(verify_message_integrity(&out[0].bytes, B_PWD.as_bytes()));
        assert_eq!(a.pairs()[0].state, PairState::InProgress);
        // Pacing: an immediate second tick sends nothing new.
        assert!(a.tick().is_empty());
    }

    #[tokio::test]
    async fn inbound_request_without_integrity_gets_400() {
        let remote: SocketAddr = "127.0.0.1:40001".parse().unwrap();
        let (mut a, local) = agent(IceRole::Controlled, 1, remote).await;
        let raw = request_from_b((Some(9), None), false)
            .encode_with(None, true)
            .unwrap();
        let reply = a.handle_datagram(&raw, remote, local).expect("reply");
        let msg = StunMessage::decode(&reply).unwrap();
        assert_eq!(msg.class, StunClass::ErrorResponse);
        assert_eq!(msg.error_code.unwrap().code, 400);
    }

    #[tokio::test]
    async fn inbound_request_with_wrong_password_gets_401() {
        let remote: SocketAddr = "127.0.0.1:40002".parse().unwrap();
        let (mut a, local) = agent(IceRole::Controlled, 1, remote).await;
        let raw = request_from_b((Some(9), None), false)
            .encode_with(Some(b"not-the-password"), true)
            .unwrap();
        let reply = a.handle_datagram(&raw, remote, local).expect("reply");
        let msg = StunMessage::decode(&reply).unwrap();
        assert_eq!(msg.class, StunClass::ErrorResponse);
        assert_eq!(msg.error_code.unwrap().code, 401);
        // Wrong username too.
        let mut req = request_from_b((Some(9), None), false);
        req.username = Some("xxxx:bbbb".into());
        let raw = req.encode_with(Some(A_PWD.as_bytes()), true).unwrap();
        let reply = a.handle_datagram(&raw, remote, local).expect("reply");
        assert_eq!(
            StunMessage::decode(&reply)
                .unwrap()
                .error_code
                .unwrap()
                .code,
            401
        );
    }

    #[tokio::test]
    async fn request_without_fingerprint_is_ignored() {
        let remote: SocketAddr = "127.0.0.1:40003".parse().unwrap();
        let (mut a, local) = agent(IceRole::Controlled, 1, remote).await;
        let raw = request_from_b((Some(9), None), false)
            .encode_with(Some(A_PWD.as_bytes()), false)
            .unwrap();
        assert!(a.handle_datagram(&raw, remote, local).is_none());
    }

    #[tokio::test]
    async fn valid_request_gets_signed_success_and_triggers_a_check() {
        let remote: SocketAddr = "127.0.0.1:40004".parse().unwrap();
        let (mut a, local) = agent(IceRole::Controlled, 1, remote).await;
        let req = request_from_b((Some(9), None), false);
        let raw = req.encode_with(Some(A_PWD.as_bytes()), true).unwrap();
        let reply = a.handle_datagram(&raw, remote, local).expect("reply");
        let msg = StunMessage::decode(&reply).unwrap();
        assert_eq!(msg.class, StunClass::SuccessResponse);
        assert_eq!(msg.transaction_id, req.transaction_id);
        assert_eq!(msg.xor_mapped_address, Some(remote));
        assert!(verify_fingerprint(&reply));
        assert!(verify_message_integrity(&reply, A_PWD.as_bytes()));
        // The triggered check goes out on the next tick.
        let out = a.tick();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].destination, remote);
        let check = StunMessage::decode(&out[0].bytes).unwrap();
        assert_eq!(check.ice_controlled, Some(1));
    }

    #[tokio::test]
    async fn request_from_unknown_address_creates_prflx_candidate_and_pair() {
        let remote: SocketAddr = "127.0.0.1:40005".parse().unwrap();
        let (mut a, local) = agent(IceRole::Controlled, 1, remote).await;
        let stranger: SocketAddr = "127.0.0.1:40999".parse().unwrap();
        let raw = request_from_b((Some(9), None), false)
            .encode_with(Some(A_PWD.as_bytes()), true)
            .unwrap();
        let reply = a.handle_datagram(&raw, stranger, local).expect("reply");
        assert_eq!(
            StunMessage::decode(&reply).unwrap().class,
            StunClass::SuccessResponse
        );
        assert_eq!(a.remote_candidates().len(), 2);
        let prflx = &a.remote_candidates()[1];
        assert_eq!(prflx.candidate_type, "prflx");
        assert_eq!(prflx.port, 40_999);
        assert_eq!(
            prflx.priority, 1_000,
            "PRIORITY attribute becomes the candidate priority"
        );
        assert_eq!(a.pairs().len(), 2);
        let out = a.tick();
        assert_eq!(
            out[0].destination, stranger,
            "triggered check goes to the new pair first"
        );
    }

    #[tokio::test]
    async fn role_conflict_larger_tie_breaker_answers_487_smaller_switches() {
        let remote: SocketAddr = "127.0.0.1:40006".parse().unwrap();
        // Both controlling; we hold the larger tie-breaker → 487.
        let (mut a, local) = agent(IceRole::Controlling, 100, remote).await;
        let raw = request_from_b((Some(50), None), false)
            .encode_with(Some(A_PWD.as_bytes()), true)
            .unwrap();
        let reply = a.handle_datagram(&raw, remote, local).expect("reply");
        let msg = StunMessage::decode(&reply).unwrap();
        assert_eq!(msg.error_code.as_ref().unwrap().code, 487);
        assert!(
            verify_message_integrity(&reply, A_PWD.as_bytes()),
            "487 must be integrity-protected"
        );
        assert_eq!(a.role(), IceRole::Controlling);

        // Smaller tie-breaker → we become controlled and answer 2xx.
        let (mut a, local) = agent(IceRole::Controlling, 10, remote).await;
        let raw = request_from_b((Some(50), None), false)
            .encode_with(Some(A_PWD.as_bytes()), true)
            .unwrap();
        let reply = a.handle_datagram(&raw, remote, local).expect("reply");
        assert_eq!(
            StunMessage::decode(&reply).unwrap().class,
            StunClass::SuccessResponse
        );
        assert_eq!(a.role(), IceRole::Controlled);
    }

    #[tokio::test]
    async fn error_401_on_our_check_fails_the_pair_and_the_agent() {
        let remote: SocketAddr = "127.0.0.1:40007".parse().unwrap();
        let (mut a, local) = agent(IceRole::Controlling, 7, remote).await;
        let out = a.tick();
        let sent = StunMessage::decode(&out[0].bytes).unwrap();
        let err = StunMessage::new_binding_error(&sent, 401, "Unauthenticated")
            .encode_with(None, true)
            .unwrap();
        assert!(a.handle_datagram(&err, remote, local).is_none());
        assert_eq!(a.pairs()[0].state, PairState::Failed);
        assert_eq!(a.state(), IceState::Failed);
        assert!(a.failure_reason().unwrap().contains("401"));
        assert!(a.tick().is_empty(), "a failed agent sends nothing");
    }

    #[tokio::test]
    async fn controlled_agent_nominates_on_use_candidate_after_its_check_succeeds() {
        let remote: SocketAddr = "127.0.0.1:40008".parse().unwrap();
        let (mut a, local) = agent(IceRole::Controlled, 1, remote).await;
        // Peer's nominating request arrives first.
        let raw = request_from_b((Some(9), None), true)
            .encode_with(Some(A_PWD.as_bytes()), true)
            .unwrap();
        a.handle_datagram(&raw, remote, local).expect("reply");
        assert_eq!(
            a.state(),
            IceState::Checking,
            "own check hasn't succeeded yet"
        );
        // Our triggered check goes out and the peer answers it.
        let out = a.tick();
        let sent = StunMessage::decode(&out[0].bytes).unwrap();
        let resp = StunMessage::new_binding_response(&sent, local)
            .encode_with(Some(B_PWD.as_bytes()), true)
            .unwrap();
        a.handle_datagram(&resp, remote, local);
        assert_eq!(a.state(), IceState::Completed);
        assert_eq!(a.selected_remote(), Some(remote));
        assert!(a.selected_pair().unwrap().nominated);
    }

    #[tokio::test]
    async fn success_response_with_bad_integrity_is_ignored() {
        let remote: SocketAddr = "127.0.0.1:40009".parse().unwrap();
        let (mut a, local) = agent(IceRole::Controlling, 7, remote).await;
        let out = a.tick();
        let sent = StunMessage::decode(&out[0].bytes).unwrap();
        let resp = StunMessage::new_binding_response(&sent, local)
            .encode_with(Some(b"wrong"), true)
            .unwrap();
        a.handle_datagram(&resp, remote, local);
        assert_eq!(
            a.pairs()[0].state,
            PairState::InProgress,
            "pair stays in progress"
        );
        assert_eq!(a.state(), IceState::Checking);
    }

    #[tokio::test]
    async fn completed_agent_sends_keepalive_indications() {
        let remote: SocketAddr = "127.0.0.1:40010".parse().unwrap();
        let (a, local) = agent(IceRole::Controlling, 7, remote).await;
        let mut a = a.with_keepalive_interval(Duration::from_millis(1));
        let out = a.tick();
        let sent = StunMessage::decode(&out[0].bytes).unwrap();
        let resp = StunMessage::new_binding_response(&sent, local)
            .encode_with(Some(B_PWD.as_bytes()), true)
            .unwrap();
        a.handle_datagram(&resp, remote, local);
        // First success → nomination check goes out next.
        let now = Instant::now() + CHECK_PACING;
        let out = a.tick_at(now);
        assert_eq!(out.len(), 1);
        let nominating = StunMessage::decode(&out[0].bytes).unwrap();
        assert!(nominating.use_candidate);
        let resp = StunMessage::new_binding_response(&nominating, local)
            .encode_with(Some(B_PWD.as_bytes()), true)
            .unwrap();
        a.handle_datagram(&resp, remote, local);
        assert_eq!(a.state(), IceState::Completed);
        // Past the keepalive interval a Binding Indication goes out.
        let out = a.tick_at(Instant::now() + Duration::from_millis(5));
        assert_eq!(out.len(), 1);
        let ka = StunMessage::decode(&out[0].bytes).unwrap();
        assert_eq!(ka.class, StunClass::Indication);
        assert!(!ka.has_message_integrity);
        assert!(verify_fingerprint(&out[0].bytes));
        assert_eq!(out[0].destination, remote);
    }

    #[tokio::test]
    async fn late_remote_candidate_revives_a_failed_agent() {
        let remote: SocketAddr = "127.0.0.1:40011".parse().unwrap();
        let (mut a, local) = agent(IceRole::Controlling, 7, remote).await;
        let out = a.tick();
        let sent = StunMessage::decode(&out[0].bytes).unwrap();
        let err = StunMessage::new_binding_error(&sent, 400, "Bad Request")
            .encode_with(None, true)
            .unwrap();
        a.handle_datagram(&err, remote, local);
        assert_eq!(a.state(), IceState::Failed);
        let trickled: SocketAddr = "127.0.0.1:40012".parse().unwrap();
        a.add_remote_candidate(&gather_host_candidates(&[trickled], 1)[0]);
        assert_eq!(a.state(), IceState::Checking);
        let out = a.tick_at(Instant::now() + CHECK_PACING);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].destination, trickled);
    }
}
