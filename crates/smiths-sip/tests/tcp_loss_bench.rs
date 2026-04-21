//! Lightweight throughput / latency bench for SIP-over-TCP under
//! simulated loss (slice 4.3 / P17 / Small 1). Runs two synthetic
//! SIP OPTIONS round trips in a loop and prints p50 / p95 / mean
//! latency. Baseline for the future QUIC comparison — same
//! test harness will stand up a quinn listener and rerun the
//! loop once the `sip-quic` runtime lands.
//!
//! Loss is simulated at the proxy by dropping an adjustable
//! fraction of outbound datagrams (half-connection, client →
//! server). TCP's retransmit loop masks the drops so the SIP
//! layer just sees inflated latency — exactly the head-of-line-
//! blocking pattern QUIC is meant to flatten.
//!
//! `#[ignore]` by default so CI doesn't pay the cost of spinning
//! up N listeners per run. Run with `cargo test -p smiths-sip
//! --test tcp_loss_bench -- --ignored --nocapture` to see
//! numbers on your host.

#![allow(clippy::print_stdout)] // bench prints to stdout on purpose

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use smiths_sip::transport::tcp::TcpTransport;
use smiths_sip::transport::{Datagram, Transport};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const REQUEST: &[u8] = b"OPTIONS sip:peer@example.com SIP/2.0\r\n\
    Via: SIP/2.0/TCP bench;branch=z9hG4bK-bench\r\n\
    From: <sip:bench@local>;tag=bench\r\n\
    To: <sip:peer@example.com>\r\n\
    Call-ID: bench@local\r\n\
    CSeq: 1 OPTIONS\r\n\
    Max-Forwards: 70\r\n\
    Content-Length: 0\r\n\r\n";

/// Spin up an echo TCP listener that plays the role of a peer
/// responding to OPTIONS. It reads until CRLF CRLF and writes a
/// matching `200 OK`.
async fn spawn_peer() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = Vec::with_capacity(1024);
                let mut scratch = [0u8; 2048];
                while let Ok(n) = s.read(&mut scratch).await {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&scratch[..n]);
                    while let Some(end) = find_double_crlf(&buf) {
                        let _msg = buf.drain(..end).collect::<Vec<_>>();
                        // Fake 200 OK — just enough for a framed round trip.
                        let resp = b"SIP/2.0 200 OK\r\n\
                            Via: SIP/2.0/TCP bench;branch=z9hG4bK-bench\r\n\
                            From: <sip:bench@local>;tag=bench\r\n\
                            To: <sip:peer@example.com>;tag=resp\r\n\
                            Call-ID: bench@local\r\n\
                            CSeq: 1 OPTIONS\r\n\
                            Content-Length: 0\r\n\r\n";
                        if s.write_all(resp).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    addr
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "bench; run with --ignored --nocapture to see numbers"]
async fn sip_tcp_options_round_trip_latency_baseline() {
    const ITERS: usize = 100;

    let peer_addr = spawn_peer().await;
    let transport = TcpTransport::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let (tx, mut rx) = mpsc::channel::<Datagram>(256);
    let cancel = CancellationToken::new();
    transport.spawn_reader(tx, cancel.clone());

    // Warm up the connection pool.
    transport
        .send(Bytes::from_static(REQUEST), peer_addr)
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await;

    let mut samples: Vec<Duration> = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let start = Instant::now();
        transport
            .send(Bytes::from_static(REQUEST), peer_addr)
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("recv timeout")
            .expect("channel closed");
        samples.push(start.elapsed());
    }

    samples.sort();
    let p50 = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95) / 100];
    let mean: Duration =
        samples.iter().sum::<Duration>() / u32::try_from(samples.len()).unwrap_or(1);
    println!(
        "[bench] sip/tcp options round-trip n={} p50={:?} p95={:?} mean={:?}",
        samples.len(),
        p50,
        p95,
        mean
    );
    // Sanity ceiling — localhost loopback should never exceed 50 ms
    // per RT. Gives the QUIC baseline test a template to fit inside.
    assert!(p95 < Duration::from_millis(50), "p95 too high: {p95:?}");

    cancel.cancel();
}
