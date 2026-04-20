//! Loopback integration: two in-process endpoints exchange a STUN
//! Binding Request/Response pair and wire the observed address into
//! an SDP candidate block. Proxy for "two engines negotiate ICE" —
//! we don't need the full UAS path to prove the primitive, just that
//! the building blocks line up.
//!
//! A future slice (trickle + re-INVITE, 1.5) upgrades this to run
//! through `smiths-testkit`'s full offer/answer harness.

use std::time::Duration;

use smiths_ice::{StunMessage, binding_ping, gather_host_candidates};
use tokio::net::UdpSocket;

#[tokio::test(flavor = "multi_thread")]
async fn two_endpoints_exchange_stun_and_emit_host_candidates() {
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server.local_addr().unwrap();

    // Minimal STUN responder — answers exactly one request with a
    // Binding Success Response.
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        let (n, from) = server.recv_from(&mut buf).await.unwrap();
        let req = StunMessage::decode(&buf[..n]).unwrap();
        let resp = StunMessage::new_binding_response(&req, from);
        server.send_to(&resp.encode().unwrap(), from).await.unwrap();
    });

    // Client side: bind a socket, gather a host candidate from it,
    // fire a Binding Request, confirm the response contains the
    // bound address in XOR-MAPPED-ADDRESS.
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
