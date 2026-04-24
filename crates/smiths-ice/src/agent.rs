use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use smiths_core::sdp::{IceParams, IceRole};
use tokio::net::UdpSocket;
use tracing::info;

use crate::stun::{StunError, StunMessage};
use smiths_sdp::IceCandidate;

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
    /// Check failed (e.g. timeout or error response).
    Failed,
}

/// A candidate pair (RFC 8445 §6.1.2.2).
#[derive(Clone, Debug)]
pub struct IcePair {
    /// Local candidate.
    pub local: IceCandidate,
    /// Remote candidate.
    pub remote: IceCandidate,
    /// Priority of the pair.
    pub priority: u64,
    /// Current state of the connectivity check.
    pub state: PairState,
    /// Whether this pair is "nominated" by the controlling agent.
    pub nominated: bool,
}

/// ICE agent state machine (RFC 8445 §6).
pub struct IceAgent {
    local_params: IceParams,
    _remote_params: IceParams,
    pairs: Vec<IcePair>,
    sockets: HashMap<SocketAddr, Arc<UdpSocket>>,
    /// Active checks being retransmitted.
    pending_checks: Vec<PendingCheck>,
}

struct PendingCheck {
    pair_idx: usize,
    msg: StunMessage,
    destination: SocketAddr,
    socket: Arc<UdpSocket>,
    last_sent: Instant,
    retransmits: u32,
}

impl IceAgent {
    /// Create a new ICE agent.
    #[must_use]
    pub fn new(
        local_params: IceParams,
        remote_params: IceParams,
        local_candidates: &[IceCandidate],
        remote_candidates: &[IceCandidate],
        sockets: HashMap<SocketAddr, Arc<UdpSocket>>,
    ) -> Self {
        let mut pairs = Vec::new();

        for local in local_candidates {
            for remote in remote_candidates {
                let (g, d) = match local_params.role {
                    IceRole::Controlling => (local.priority, remote.priority),
                    IceRole::Controlled => (remote.priority, local.priority),
                };
                let priority =
                    (u64::from(g.min(d)) << 32) + (u64::from(g.max(d)) << 1) + u64::from(g > d);

                pairs.push(IcePair {
                    local: local.clone(),
                    remote: remote.clone(),
                    priority,
                    state: PairState::Frozen,
                    nominated: false,
                });
            }
        }

        pairs.sort_by_key(|b| std::cmp::Reverse(b.priority));

        for pair in &mut pairs {
            if sockets.contains_key(&SocketAddr::new(pair.local.address, pair.local.port)) {
                pair.state = PairState::Waiting;
            }
        }

        Self {
            local_params,
            _remote_params: remote_params,
            pairs,
            sockets,
            pending_checks: Vec::new(),
        }
    }

    /// Process an incoming STUN message.
    pub fn handle_stun(
        &mut self,
        msg: &StunMessage,
        source: SocketAddr,
    ) -> Result<Option<StunMessage>, StunError> {
        match (msg.class, msg.method) {
            (crate::stun::StunClass::Request, crate::stun::StunMethod::Binding) => {
                let resp = StunMessage::new_binding_response(msg, source);
                Ok(Some(resp))
            }
            (crate::stun::StunClass::SuccessResponse, crate::stun::StunMethod::Binding) => {
                if let Some(pos) = self
                    .pending_checks
                    .iter()
                    .position(|c| c.msg.transaction_id == msg.transaction_id)
                {
                    let check = self.pending_checks.remove(pos);
                    let pair = &mut self.pairs[check.pair_idx];
                    if SocketAddr::new(pair.remote.address, pair.remote.port) == source {
                        info!(?pair.local, ?pair.remote, "ICE check succeeded");
                        pair.state = PairState::Succeeded;
                        if self.local_params.role == IceRole::Controlling && !pair.nominated {
                            pair.nominated = true;
                        }
                    }
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Update the agent state, returning checks that need to be sent.
    pub fn tick(&mut self) -> Vec<(StunMessage, SocketAddr, Arc<UdpSocket>)> {
        let mut to_send = Vec::new();
        let now = Instant::now();

        // 1. Retransmit pending checks.
        for check in &mut self.pending_checks {
            // RFC 8489 §14.3: RTO doubles each time. Base 500ms.
            let timeout = Duration::from_millis(500 * (1 << check.retransmits));
            if now.duration_since(check.last_sent) > timeout && check.retransmits < 7 {
                // Max 7 retransmits per RFC
                check.retransmits += 1;
                check.last_sent = now;
                to_send.push((check.msg.clone(), check.destination, check.socket.clone()));
            }
        }

        // 2. Start new checks if needed.
        // For MVP, we limit concurrent checks to 1 per pair.
        for i in 0..self.pairs.len() {
            if self.pairs[i].state == PairState::Waiting
                && !self.pending_checks.iter().any(|c| c.pair_idx == i)
                && let Some((msg, dst, sock)) = self.build_check(i)
            {
                self.pairs[i].state = PairState::InProgress;
                self.pending_checks.push(PendingCheck {
                    pair_idx: i,
                    msg: msg.clone(),
                    destination: dst,
                    socket: sock.clone(),
                    last_sent: now,
                    retransmits: 0,
                });
                to_send.push((msg, dst, sock));
            }
        }

        to_send
    }

    fn build_check(&self, pair_idx: usize) -> Option<(StunMessage, SocketAddr, Arc<UdpSocket>)> {
        let pair = &self.pairs[pair_idx];
        let socket = self
            .sockets
            .get(&SocketAddr::new(pair.local.address, pair.local.port))?
            .clone();

        let mut msg = StunMessage::new_binding_request();
        msg.priority = Some(pair.local.priority);
        if self.local_params.role == IceRole::Controlling {
            msg.ice_controlling = Some(self.local_params.tie_breaker);
            msg.use_candidate = true;
        } else {
            msg.ice_controlled = Some(self.local_params.tie_breaker);
        }

        Some((
            msg,
            SocketAddr::new(pair.remote.address, pair.remote.port),
            socket,
        ))
    }
}
