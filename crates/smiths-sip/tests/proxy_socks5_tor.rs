//! Integration test for the SOCKS5 outbound path, driven by a real
//! SOCKS5 proxy. Skipped unless `TOR_SOCKS_PROXY` is set in the
//! environment — CI images that ship Tor (or any other SOCKS5
//! server) export `TOR_SOCKS_PROXY=127.0.0.1:9050`.
//!
//! The test opens a TCP listener that plays the role of a SIP peer,
//! configures the engine's `TcpTransport` with a `Socks5Connector`
//! pointed at the proxy, and sends one SIP OPTIONS message. A
//! successful handshake + delivery proves the proxy path carries
//! framed SIP bytes end-to-end; Tor in particular won't route to
//! `127.0.0.1` unless `AllowPrivateRange` is set, so CI operators
//! typically point `TOR_SOCKS_PROXY` at a lightweight SOCKS server
//! like `shadowsocks-libev` or `microsocks` for this check.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use smiths_sip::transport::proxy::Socks5Connector;
use smiths_sip::{TcpTransport, Transport as _};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn proxy_addr() -> Option<SocketAddr> {
    let raw = std::env::var("TOR_SOCKS_PROXY").ok()?;
    raw.parse().ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn sip_options_over_socks5_proxy_round_trip() {
    let Some(proxy) = proxy_addr() else {
        // TOR_SOCKS_PROXY not set — treat this test as ignored.
        return;
    };

    // Stand up a receiver pretending to be the far-side SIP peer.
    let peer_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer_listener.local_addr().unwrap();

    let received = tokio::spawn(async move {
        let (mut s, _) = peer_listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut scratch = [0u8; 1024];
        loop {
            match s.read(&mut scratch).await.unwrap() {
                0 => break,
                n => {
                    buf.extend_from_slice(&scratch[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        s.shutdown().await.ok();
        buf
    });

    // Build the engine-side transport, wire the SOCKS5 connector,
    // and `send` one synthetic SIP OPTIONS line to the peer.
    let transport = TcpTransport::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap()
        .with_proxy(Arc::new(Socks5Connector::new(proxy, None)));
    let (tx, _rx) = mpsc::channel(4);
    let cancel = CancellationToken::new();
    transport.spawn_reader(tx, cancel.clone());

    let options = Bytes::from_static(
        b"OPTIONS sip:peer@example.com SIP/2.0\r\n\
          Content-Length: 0\r\n\r\n",
    );
    transport.send(options, peer_addr).await.unwrap();

    let got = received.await.unwrap();
    let as_str = String::from_utf8(got).unwrap();
    assert!(
        as_str.starts_with("OPTIONS sip:peer@example.com SIP/2.0"),
        "expected OPTIONS line, got: {as_str:?}"
    );
    cancel.cancel();
}
