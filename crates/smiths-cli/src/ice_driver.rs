//! Driving one WebRTC leg's ICE agent to a selected candidate pair.
//!
//! [`smiths_ice::IceAgent`] is sans-IO: it decides what to send and
//! what an inbound STUN message means, and the caller owns the socket.
//! This is that caller. It runs **before** DTLS and owns the media
//! socket exclusively for the duration, so nothing races it for
//! datagrams; once a pair is nominated the socket is handed back and
//! DTLS runs against the address ICE actually validated rather than
//! whatever the offer's `c=` line claimed.
//!
//! Non-STUN datagrams that arrive mid-check are dropped. A peer that
//! starts its DTLS `ClientHello` before we nominate will retransmit it
//! — DTLS carries its own retransmission timer for exactly this — so
//! dropping is cheaper than buffering bytes we cannot hand to the
//! handshake anyway.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use smiths_core::Metrics;
use smiths_core::sdp::IceParams;
use smiths_ice::agent::{IceAgent, IceState};
use smiths_ice::stun::is_stun;
use smiths_sdp::types::IceCandidate;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// How often the agent is polled while checks are in flight. RFC 8445
/// §14.2's default pacing is 50 ms, and the agent decides internally
/// whether a given tick actually emits anything.
const TICK_INTERVAL: Duration = Duration::from_millis(50);

/// Largest datagram the driver will read while ICE owns the socket.
const MAX_DATAGRAM: usize = 2048;

/// Why connectivity establishment ended without a usable pair.
///
/// Hand-written rather than derived: this is the binary crate, which
/// keeps `thiserror` out of its dependency list by convention.
#[derive(Debug)]
pub(crate) enum IceError {
    /// The agent exhausted its pairs; the string is
    /// `IceAgent::failure_reason`.
    Failed(String),
    /// No pair was nominated inside the deadline.
    Timeout(Duration),
    /// The socket broke while checks were running.
    Io(std::io::Error),
}

impl std::fmt::Display for IceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(why) => write!(f, "ICE failed: {why}"),
            Self::Timeout(d) => write!(f, "ICE did not complete within {d:?}"),
            Self::Io(e) => write!(f, "ICE socket error: {e}"),
        }
    }
}

impl std::error::Error for IceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Failed(_) | Self::Timeout(_) => None,
        }
    }
}

impl From<std::io::Error> for IceError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// A live agent a trickled candidate can still be added to.
pub(crate) type SharedAgent = Arc<Mutex<IceAgent>>;

/// Build the agent for one leg.
///
/// `socket` is the media endpoint's own socket, so the pair ICE
/// validates is the 5-tuple RTP will later use. Local candidates must
/// include the socket's own address or the agent has nothing to check
/// from.
pub(crate) fn build_agent(
    params: IceParams,
    local: &[IceCandidate],
    remote: &[IceCandidate],
    socket: &Arc<UdpSocket>,
    metrics: Option<Arc<Metrics>>,
) -> std::io::Result<SharedAgent> {
    let base = socket.local_addr()?;
    let mut sockets = HashMap::new();
    sockets.insert(base, Arc::clone(socket));
    // Server-reflexive candidates are reached through the same base
    // socket, so they map to it too; without this the agent drops
    // every srflx pair for want of a socket to send from.
    for c in local {
        let addr = SocketAddr::new(c.address, c.port);
        sockets.entry(addr).or_insert_with(|| Arc::clone(socket));
    }
    let mut agent = IceAgent::new(params, local, remote, sockets);
    if let Some(m) = metrics {
        agent = agent.with_metrics(m);
    }
    Ok(Arc::new(Mutex::new(agent)))
}

/// Run connectivity checks until a pair is nominated or the agent
/// gives up, and return the validated remote address.
///
/// # Errors
///
/// [`IceError::Failed`] when the agent exhausts its pairs,
/// [`IceError::Timeout`] when `deadline` elapses first.
pub(crate) async fn run_to_completion(
    agent: &SharedAgent,
    socket: &Arc<UdpSocket>,
    deadline: Duration,
) -> Result<SocketAddr, IceError> {
    let give_up = tokio::time::Instant::now() + deadline;
    let mut buf = vec![0u8; MAX_DATAGRAM];

    loop {
        // Emit whatever the agent wants sent right now.
        let outgoing = {
            let mut a = agent.lock().await;
            a.tick()
        };
        for out in outgoing {
            if let Err(e) = out.socket.send_to(&out.bytes, out.destination).await {
                debug!(dest = %out.destination, ?e, "ICE check send failed");
            }
        }

        // Terminal states end the loop before we wait on the socket
        // again, so a completed agent releases it promptly.
        {
            let a = agent.lock().await;
            match a.state() {
                IceState::Completed => {
                    if let Some(addr) = a.selected_remote() {
                        debug!(%addr, "ICE selected a candidate pair");
                        return Ok(addr);
                    }
                    // `Completed` without a selected pair would be an
                    // agent bug; treat it as a failure rather than
                    // spinning.
                    return Err(IceError::Failed(
                        "agent reported Completed with no selected pair".into(),
                    ));
                }
                IceState::Failed => {
                    return Err(IceError::Failed(
                        a.failure_reason().unwrap_or("no reason given").to_owned(),
                    ));
                }
                IceState::Checking => {}
            }
        }

        if tokio::time::Instant::now() >= give_up {
            return Err(IceError::Timeout(deadline));
        }

        // Wait for the next datagram or the next tick, whichever comes
        // first. Only STUN reaches the agent.
        match tokio::time::timeout(TICK_INTERVAL, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                if is_stun(&buf[..n]) {
                    let reply = {
                        let mut a = agent.lock().await;
                        a.handle_datagram(&buf[..n], from, socket.local_addr()?)
                    };
                    if let Some(reply) = reply
                        && let Err(e) = socket.send_to(&reply, from).await
                    {
                        debug!(%from, ?e, "ICE check reply send failed");
                    }
                } else {
                    debug!(
                        %from,
                        bytes = n,
                        "dropping non-STUN datagram while ICE checks run"
                    );
                }
            }
            Ok(Err(e)) => return Err(IceError::Io(e)),
            // Tick deadline: loop round and let the agent emit again.
            Err(_) => {}
        }
    }
}

/// Add a trickled remote candidate to a live agent.
pub(crate) async fn add_remote_candidate(agent: &SharedAgent, candidate: &IceCandidate) {
    let mut a = agent.lock().await;
    a.add_remote_candidate(candidate);
}

/// Keep the nominated pair alive after ICE completes.
///
/// RFC 8445 §11 keepalives are STUN Binding *indications*: they need
/// no response, so this task only sends. That matters because DTLS and
/// then the RTP bridge own the socket's read side by this point —
/// a keepalive task that also read would steal their datagrams.
pub(crate) fn spawn_keepalives(
    agent: SharedAgent,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            let outgoing = {
                let mut a = agent.lock().await;
                a.tick()
            };
            for out in outgoing {
                if let Err(e) = out.socket.send_to(&out.bytes, out.destination).await {
                    warn!(dest = %out.destination, ?e, "ICE keepalive send failed");
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smiths_core::sdp::IceRole;
    use smiths_ice::candidate::gather_host_candidates;

    async fn loopback_socket() -> Arc<UdpSocket> {
        Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"))
    }

    fn params(local_ufrag: &str, remote_ufrag: &str, pwd: &str, role: IceRole) -> IceParams {
        IceParams {
            local_ufrag: local_ufrag.into(),
            local_pwd: "localpasswordlocalpwd".into(),
            remote_ufrag: remote_ufrag.into(),
            remote_pwd: pwd.into(),
            role,
            tie_breaker: 42,
        }
    }

    /// Two real agents over loopback: the driver must nominate the
    /// peer's address, not whatever was in the offer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn driver_completes_against_a_real_peer() {
        const PWD_A: &str = "passwordForAgentAxx";
        const PWD_B: &str = "passwordForAgentBxx";
        let a_sock = loopback_socket().await;
        let b_sock = loopback_socket().await;
        let a_addr = a_sock.local_addr().expect("addr");
        let b_addr = b_sock.local_addr().expect("addr");

        let a_cands = gather_host_candidates(&[a_addr], 1);
        let b_cands = gather_host_candidates(&[b_addr], 1);

        let a = build_agent(
            IceParams {
                local_ufrag: "agentA".into(),
                local_pwd: PWD_A.into(),
                remote_ufrag: "agentB".into(),
                remote_pwd: PWD_B.into(),
                role: IceRole::Controlling,
                tie_breaker: 1,
            },
            &a_cands,
            &b_cands,
            &a_sock,
            None,
        )
        .expect("agent a");
        let b = build_agent(
            IceParams {
                local_ufrag: "agentB".into(),
                local_pwd: PWD_B.into(),
                remote_ufrag: "agentA".into(),
                remote_pwd: PWD_A.into(),
                role: IceRole::Controlled,
                tie_breaker: 2,
            },
            &b_cands,
            &a_cands,
            &b_sock,
            None,
        )
        .expect("agent b");

        let b_task = {
            let b_sock = Arc::clone(&b_sock);
            tokio::spawn(
                async move { run_to_completion(&b, &b_sock, Duration::from_secs(10)).await },
            )
        };
        let selected = run_to_completion(&a, &a_sock, Duration::from_secs(10))
            .await
            .expect("controlling agent completes");
        assert_eq!(selected, b_addr, "media must go to the validated address");
        let _ = tokio::time::timeout(Duration::from_secs(5), b_task).await;
    }

    /// A peer whose password does not match must never be selected:
    /// the integrity check fails and the agent gives up.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn wrong_password_peer_never_completes() {
        let a_sock = loopback_socket().await;
        let b_sock = loopback_socket().await;
        let a_addr = a_sock.local_addr().expect("addr");
        let b_addr = b_sock.local_addr().expect("addr");
        let a_cands = gather_host_candidates(&[a_addr], 1);
        let b_cands = gather_host_candidates(&[b_addr], 1);

        let a = build_agent(
            params(
                "agentA",
                "agentB",
                "theWrongPasswordXx",
                IceRole::Controlling,
            ),
            &a_cands,
            &b_cands,
            &a_sock,
            None,
        )
        .expect("agent a");
        let b = build_agent(
            params(
                "agentB",
                "agentA",
                "adifferentPasswordX",
                IceRole::Controlled,
            ),
            &b_cands,
            &a_cands,
            &b_sock,
            None,
        )
        .expect("agent b");

        let b_task = {
            let b_sock = Arc::clone(&b_sock);
            tokio::spawn(
                async move { run_to_completion(&b, &b_sock, Duration::from_secs(2)).await },
            )
        };
        let err = run_to_completion(&a, &a_sock, Duration::from_secs(2))
            .await
            .expect_err("a mismatched password must not yield a pair");
        assert!(
            matches!(err, IceError::Failed(_) | IceError::Timeout(_)),
            "unexpected error: {err}"
        );
        let _ = tokio::time::timeout(Duration::from_secs(5), b_task).await;
    }

    /// A candidate that arrives after the agent was built still forms
    /// a pair and can win.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn trickled_candidate_can_win_the_pair() {
        const PWD_A: &str = "passwordForAgentAxx";
        const PWD_B: &str = "passwordForAgentBxx";
        let a_sock = loopback_socket().await;
        let b_sock = loopback_socket().await;
        let a_addr = a_sock.local_addr().expect("addr");
        let b_addr = b_sock.local_addr().expect("addr");
        let a_cands = gather_host_candidates(&[a_addr], 1);
        let b_cands = gather_host_candidates(&[b_addr], 1);

        // A starts with *no* remote candidates; B's arrives by trickle.
        let a = build_agent(
            IceParams {
                local_ufrag: "agentA".into(),
                local_pwd: PWD_A.into(),
                remote_ufrag: "agentB".into(),
                remote_pwd: PWD_B.into(),
                role: IceRole::Controlling,
                tie_breaker: 1,
            },
            &a_cands,
            &[],
            &a_sock,
            None,
        )
        .expect("agent a");
        let b = build_agent(
            IceParams {
                local_ufrag: "agentB".into(),
                local_pwd: PWD_B.into(),
                remote_ufrag: "agentA".into(),
                remote_pwd: PWD_A.into(),
                role: IceRole::Controlled,
                tie_breaker: 2,
            },
            &b_cands,
            &a_cands,
            &b_sock,
            None,
        )
        .expect("agent b");

        add_remote_candidate(&a, &b_cands[0]).await;

        let b_task = {
            let b_sock = Arc::clone(&b_sock);
            tokio::spawn(
                async move { run_to_completion(&b, &b_sock, Duration::from_secs(10)).await },
            )
        };
        let selected = run_to_completion(&a, &a_sock, Duration::from_secs(10))
            .await
            .expect("the trickled candidate forms a usable pair");
        assert_eq!(selected, b_addr);
        let _ = tokio::time::timeout(Duration::from_secs(5), b_task).await;
    }
}
