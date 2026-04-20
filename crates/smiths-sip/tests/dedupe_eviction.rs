//! Regression test for the UAS dedupe-eviction deadlock.
//!
//! Before the v0.13.1 bugfix, `UasServer::respond` evicted a cache
//! entry via `self.dedupe.iter().next()` + `remove(&k)`. Rust's
//! `if let` extends the `iter()` rvalue temporary through the full
//! scope, so the `Iter` (holding a `DashMap` shard read guard) was
//! still alive when `remove` took a write lock on the same shard —
//! the UAS wedged permanently once `DEDUPE_CAPACITY` (4096) was hit.
//!
//! This test sends > 4096 unique OPTIONS requests (each with its own
//! Via branch so the dedupe cache grows) at the UAS and asserts that
//! all of them get a 200 OK, including ones past the eviction
//! threshold. Under the pre-fix code, sending > 4096 unique branches
//! leaves the UAS stuck and the last batch times out.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use smiths_core::{EventBus, MediaFabric, SdpNegotiator};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Negotiator;
use smiths_sip::{Transport as _, UasServer, UdpTransport};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

async fn spawn_uas() -> SocketAddr {
    let t = UdpTransport::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let local = t.local_addr().unwrap();
    let transport = Arc::new(t);
    let bus = EventBus::new(16);
    let cancel = CancellationToken::new();
    let (tx, rx) = mpsc::channel(8192);
    transport.spawn_reader(tx, cancel.clone());
    let fabric: Arc<dyn MediaFabric> = Arc::new(UdpMediaFabric::new());
    let negotiator: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(local.ip()));
    let server = UasServer::new(Arc::clone(&transport), bus, fabric, negotiator).unwrap();
    tokio::spawn(server.run(rx, cancel));
    local
}

fn options_with_branch(ca: SocketAddr, branch: &str) -> Vec<u8> {
    format!(
        concat!(
            "OPTIONS sip:alice@127.0.0.1 SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch={branch};rport\r\n",
            "From: Tester <sip:tester@127.0.0.1>;tag=tst\r\n",
            "To: Target <sip:alice@127.0.0.1>\r\n",
            "Call-ID: cid-dedupe-{branch}@127.0.0.1\r\n",
            "CSeq: 1 OPTIONS\r\n",
            "Max-Forwards: 70\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ca = ca,
        branch = branch,
    )
    .into_bytes()
}

/// Send `count` OPTIONS with distinct Via branches and count the
/// number of 200 OKs the UAS returns within `recv_budget`.
async fn drive_unique_options(
    uas: SocketAddr,
    count: usize,
    recv_budget: Duration,
) -> (usize, usize) {
    let client = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let ca = client.local_addr().unwrap();

    // Receiver task counts 200 OKs until `recv_budget` lapses.
    let recv_client = Arc::clone(&client);
    let recv = tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        let mut ok = 0usize;
        let deadline = tokio::time::Instant::now() + recv_budget;
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline - tokio::time::Instant::now();
            match timeout(remaining, recv_client.recv_from(&mut buf)).await {
                Ok(Ok((n, _))) => {
                    if buf[..n].starts_with(b"SIP/2.0 200 OK\r\n") {
                        ok += 1;
                    }
                }
                _ => break,
            }
        }
        ok
    });

    let mut sent = 0usize;
    for i in 0..count {
        let branch = format!("z9hG4bK-dedupe-{i}");
        let body = options_with_branch(ca, &branch);
        if client.send_to(&body, uas).await.is_ok() {
            sent += 1;
        }
        // Tiny pacing so we don't overrun the UAS's channel buffer on
        // the test host. At ~1 ms per send we complete 4200 sends in
        // ~4 s, well inside the recv_budget.
        if i % 256 == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    let ok = recv.await.unwrap();
    (sent, ok)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uas_keeps_responding_after_dedupe_capacity_eviction() {
    // DEDUPE_CAPACITY is 4096 in the UAS. Drive enough unique
    // branches to force at least a dozen evictions — if the eviction
    // path deadlocks, no 200s come back after the 4096th.
    let uas = spawn_uas().await;
    let (sent, ok) = drive_unique_options(uas, 4200, Duration::from_secs(10)).await;
    assert_eq!(sent, 4200, "all sends should succeed");
    assert!(
        ok >= 4100,
        "expected ~4200 200 OKs, got {ok} (sent={sent}) — dedupe eviction likely deadlocked"
    );
}
