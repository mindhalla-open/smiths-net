//! Loopback integration: two in-process ICE agents on real UDP
//! sockets run authenticated connectivity checks to completion, a
//! wrong password makes the checks fail, and srflx gathering works
//! against a local STUN responder.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use smiths_core::sdp::{IceParams, IceRole};
use smiths_ice::{
    CandidateGatherer, IceAgent, IceState, StunMessage, binding_ping, gather_host_candidates,
    gather_srflx_candidates, is_stun,
};
use tokio::net::UdpSocket;

const A_UFRAG: &str = "agentA";
const A_PWD: &str = "agentApasswordagentApwd";
const B_UFRAG: &str = "agentB";
const B_PWD: &str = "agentBpasswordagentBpwd";

struct Endpoint {
    agent: IceAgent,
    socket: Arc<UdpSocket>,
    addr: SocketAddr,
}

fn endpoint(params: IceParams, socket: Arc<UdpSocket>, remote: SocketAddr) -> Endpoint {
    let addr = socket.local_addr().unwrap();
    let local = gather_host_candidates(&[addr], 1);
    let remote_cands = gather_host_candidates(&[remote], 1);
    let mut sockets = HashMap::new();
    sockets.insert(addr, Arc::clone(&socket));
    Endpoint {
        agent: IceAgent::new(params, &local, &remote_cands, sockets),
        socket,
        addr,
    }
}

/// One driver iteration for `ep`: send whatever the agent wants
/// out, then drain inbound datagrams for a few milliseconds.
async fn pump(ep: &mut Endpoint) {
    for out in ep.agent.tick() {
        out.socket
            .send_to(&out.bytes, out.destination)
            .await
            .unwrap();
    }
    let mut buf = [0u8; 1500];
    loop {
        match tokio::time::timeout(Duration::from_millis(5), ep.socket.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                assert!(is_stun(&buf[..n]), "only STUN flows in this test");
                if let Some(reply) = ep.agent.handle_datagram(&buf[..n], from, ep.addr) {
                    ep.socket.send_to(&reply, from).await.unwrap();
                }
            }
            Ok(Err(e)) => panic!("recv failed: {e}"),
            Err(_) => break,
        }
    }
}

async fn run_until<F: Fn(&Endpoint, &Endpoint) -> bool>(
    a: &mut Endpoint,
    b: &mut Endpoint,
    done: F,
    max_iterations: usize,
) -> bool {
    for _ in 0..max_iterations {
        pump(a).await;
        pump(b).await;
        if done(a, b) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn two_agents_complete_authenticated_checks_and_nominate_a_pair() {
    let sock_a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let sock_b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr_a = sock_a.local_addr().unwrap();
    let addr_b = sock_b.local_addr().unwrap();

    let params_a = IceParams {
        local_ufrag: A_UFRAG.into(),
        local_pwd: A_PWD.into(),
        remote_ufrag: B_UFRAG.into(),
        remote_pwd: B_PWD.into(),
        role: IceRole::Controlling,
        tie_breaker: 1_000,
    };
    let params_b = IceParams {
        local_ufrag: B_UFRAG.into(),
        local_pwd: B_PWD.into(),
        remote_ufrag: A_UFRAG.into(),
        remote_pwd: A_PWD.into(),
        role: IceRole::Controlled,
        tie_breaker: 2_000,
    };
    let mut a = endpoint(params_a, sock_a, addr_b);
    let mut b = endpoint(params_b, sock_b, addr_a);

    let completed = run_until(
        &mut a,
        &mut b,
        |a, b| a.agent.state() == IceState::Completed && b.agent.state() == IceState::Completed,
        200,
    )
    .await;
    assert!(
        completed,
        "both agents must complete: a={:?} b={:?}",
        a.agent.state(),
        b.agent.state()
    );
    assert_eq!(a.agent.selected_remote(), Some(addr_b));
    assert_eq!(b.agent.selected_remote(), Some(addr_a));
    assert!(a.agent.selected_pair().unwrap().nominated);
    assert!(b.agent.selected_pair().unwrap().nominated);
    // Loopback: the peer sees us at our bound address, so the
    // mapped address on the selected pair is our own socket.
    assert_eq!(
        a.agent.selected_pair().unwrap().mapped_address,
        Some(addr_a)
    );
    assert_eq!(
        b.agent.selected_pair().unwrap().mapped_address,
        Some(addr_b)
    );
    assert_eq!(a.agent.role(), IceRole::Controlling);
    assert_eq!(b.agent.role(), IceRole::Controlled);
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_password_fails_the_checks() {
    let sock_a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let sock_b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr_a = sock_a.local_addr().unwrap();
    let addr_b = sock_b.local_addr().unwrap();

    // A signs its checks with a password B never issued.
    let params_a = IceParams {
        local_ufrag: A_UFRAG.into(),
        local_pwd: A_PWD.into(),
        remote_ufrag: B_UFRAG.into(),
        remote_pwd: "definitely-not-agent-b-pwd".into(),
        role: IceRole::Controlling,
        tie_breaker: 1_000,
    };
    let params_b = IceParams {
        local_ufrag: B_UFRAG.into(),
        local_pwd: B_PWD.into(),
        remote_ufrag: A_UFRAG.into(),
        remote_pwd: A_PWD.into(),
        role: IceRole::Controlled,
        tie_breaker: 2_000,
    };
    let mut a = endpoint(params_a, sock_a, addr_b);
    let mut b = endpoint(params_b, sock_b, addr_a);

    let failed = run_until(
        &mut a,
        &mut b,
        |a, _| a.agent.state() == IceState::Failed,
        200,
    )
    .await;
    assert!(
        failed,
        "A's checks are rejected with 401 and the agent fails"
    );
    assert!(
        a.agent.failure_reason().unwrap().contains("401"),
        "reason: {:?}",
        a.agent.failure_reason()
    );
    // B's own checks toward A pass (they are signed with A's real
    // password) but the controlling side never nominates, so B is
    // neither completed nor failed.
    assert_eq!(b.agent.state(), IceState::Checking);
    assert!(b.agent.selected_remote().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn role_conflict_between_two_controlling_agents_resolves() {
    let sock_a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let sock_b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr_a = sock_a.local_addr().unwrap();
    let addr_b = sock_b.local_addr().unwrap();

    let params_a = IceParams {
        local_ufrag: A_UFRAG.into(),
        local_pwd: A_PWD.into(),
        remote_ufrag: B_UFRAG.into(),
        remote_pwd: B_PWD.into(),
        role: IceRole::Controlling,
        tie_breaker: 10,
    };
    let params_b = IceParams {
        local_ufrag: B_UFRAG.into(),
        local_pwd: B_PWD.into(),
        remote_ufrag: A_UFRAG.into(),
        remote_pwd: A_PWD.into(),
        role: IceRole::Controlling,
        tie_breaker: 20,
    };
    let mut a = endpoint(params_a, sock_a, addr_b);
    let mut b = endpoint(params_b, sock_b, addr_a);

    let completed = run_until(
        &mut a,
        &mut b,
        |a, b| a.agent.state() == IceState::Completed && b.agent.state() == IceState::Completed,
        200,
    )
    .await;
    assert!(completed, "a={:?} b={:?}", a.agent.state(), b.agent.state());
    // The larger tie-breaker keeps the controlling role.
    assert_eq!(a.agent.role(), IceRole::Controlled);
    assert_eq!(b.agent.role(), IceRole::Controlling);
}

/// Minimal STUN responder: answers every Binding Request with the
/// caller's source address in `XOR-MAPPED-ADDRESS`.
async fn stun_responder() -> SocketAddr {
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        while let Ok((n, from)) = server.recv_from(&mut buf).await {
            if let Ok(req) = StunMessage::decode(&buf[..n]) {
                let resp = StunMessage::new_binding_response(&req, from);
                let _ = server.send_to(&resp.encode().unwrap(), from).await;
            }
        }
    });
    server_addr
}

#[tokio::test(flavor = "multi_thread")]
async fn binding_ping_and_host_candidate_agree_on_loopback() {
    let server_addr = stun_responder().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let client_addr = client.local_addr().unwrap();

    let candidates = gather_host_candidates(&[client_addr], 1);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].address, client_addr.ip());
    assert_eq!(candidates[0].port, client_addr.port());
    assert_eq!(candidates[0].candidate_type, "host");

    let observed = binding_ping(&client, server_addr, Duration::from_millis(500))
        .await
        .expect("loopback STUN check should complete");
    assert_eq!(
        observed, client_addr,
        "STUN server must echo our bound socket address"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn srflx_gathering_against_local_stun_servers() {
    let server_1 = stun_responder().await;
    let server_2 = stun_responder().await;
    let silent: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let local = socket.local_addr().unwrap();

    // Two responders agree on the observed address → one srflx
    // address; the silent server is skipped.
    let observed = gather_srflx_candidates(
        &socket,
        &[server_1, server_2, silent],
        Duration::from_millis(300),
    )
    .await;
    assert_eq!(observed, vec![local]);

    let gathered = CandidateGatherer::new()
        .with_bind(local)
        .gather_all(&socket, &[server_1], None, 1)
        .await
        .unwrap();
    assert!(gathered.turn.is_none());
    assert_eq!(gathered.candidates.len(), 2, "host + srflx");
    assert_eq!(gathered.candidates[0].candidate_type, "host");
    let srflx = &gathered.candidates[1];
    assert_eq!(srflx.candidate_type, "srflx");
    assert_eq!(srflx.address, local.ip());
    assert_eq!(srflx.port, local.port());
    assert_eq!(srflx.related_address, Some(local.ip()));
    assert_eq!(srflx.related_port, Some(local.port()));
    assert!(srflx.priority < gathered.candidates[0].priority);
}
