//! End-to-end DTLS-SRTP handshake drive-through (slice 5.10-dtls).
//!
//! Binds two fabric endpoints on loopback, runs a real DTLS
//! handshake between them via
//! [`UdpMediaFabric::run_dtls_handshake`], and asserts that both
//! sides derive mirror-image SRTP keys per RFC 5764 §4.2. One
//! side plays `Client` (the `active` role); the other plays
//! `Server` (`passive`). The test doubles as the integration
//! surface for slice 5.10-dtls Small 1 — it exercises the
//! `classify_error` + metrics wiring at the `Success` leaf.

use std::net::IpAddr;
use std::sync::Arc;

use smiths_core::metrics::WebRtcDtlsOutcomeLabel;
use smiths_core::{MediaFabric, Metrics, SelfSignedCert};
use smiths_dtls::{DtlsLegConfig, DtlsRole};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Fingerprint;

#[tokio::test(flavor = "multi_thread")]
async fn loopback_handshake_derives_mirror_srtp_keys() {
    let metrics = {
        let mut registry = prometheus_client::registry::Registry::default();
        Metrics::register(&mut registry)
    };

    // One fabric per role so each side owns its own socket and
    // metric-increment path. Production wiring shares a single
    // fabric; splitting the pair here exercises the shape we
    // expect each engine to have in a two-party call.
    let client_fabric = Arc::new(UdpMediaFabric::new().with_metrics(Arc::clone(&metrics)));
    let server_fabric = Arc::new(UdpMediaFabric::new().with_metrics(Arc::clone(&metrics)));

    let client_endpoint = client_fabric
        .allocate(IpAddr::from([127, 0, 0, 1]))
        .await
        .expect("client endpoint");
    let server_endpoint = server_fabric
        .allocate(IpAddr::from([127, 0, 0, 1]))
        .await
        .expect("server endpoint");
    let client_addr = client_endpoint.local_addr();
    let server_addr = server_endpoint.local_addr();

    let client_cert = SelfSignedCert::generate("client").expect("client cert");
    let server_cert = SelfSignedCert::generate("server").expect("server cert");

    let client_cfg = DtlsLegConfig {
        local_cert: client_cert.clone(),
        role: DtlsRole::Client,
        peer_fingerprint: Fingerprint {
            algorithm: "sha-256".into(),
            value: server_cert.sha256_fingerprint.clone(),
        },
    };
    let server_cfg = DtlsLegConfig {
        local_cert: server_cert.clone(),
        role: DtlsRole::Server,
        peer_fingerprint: Fingerprint {
            algorithm: "sha-256".into(),
            value: client_cert.sha256_fingerprint.clone(),
        },
    };

    // Spawn both sides concurrently — a DTLS handshake is a
    // synchronous back-and-forth, running them sequentially
    // deadlocks on the first ClientHello.
    let client_fabric_task = Arc::clone(&client_fabric);
    let server_fabric_task = Arc::clone(&server_fabric);
    let client_id = client_endpoint.id();
    let server_id = server_endpoint.id();
    let client_handle = tokio::spawn(async move {
        client_fabric_task
            .run_dtls_handshake(client_id, server_addr, client_cfg)
            .await
    });
    let server_handle = tokio::spawn(async move {
        server_fabric_task
            .run_dtls_handshake(server_id, client_addr, server_cfg)
            .await
    });

    let (client_res, server_res) =
        tokio::try_join!(client_handle, server_handle).expect("join handshake tasks");
    let client_keys = client_res.expect("client handshake").srtp;
    let server_keys = server_res.expect("server handshake").srtp;

    // RFC 5764 §4.2: server's peer-tx is the client's local-tx.
    // If this assertion fails, the key split is wrong and every
    // decrypted packet would land in the wrong direction.
    assert_eq!(
        client_keys.suite, server_keys.suite,
        "both sides must agree on the SRTP suite"
    );
    assert_eq!(
        client_keys.local_tx_key, server_keys.peer_tx_key,
        "client egress key must equal server ingress key"
    );
    assert_eq!(
        client_keys.peer_tx_key, server_keys.local_tx_key,
        "server egress key must equal client ingress key"
    );
    assert_ne!(
        client_keys.local_tx_key, client_keys.peer_tx_key,
        "ingress and egress halves must differ — otherwise DTLS key split is broken"
    );

    // Metric: both sides should have bumped
    // `smiths_webrtc_dtls_handshakes_total{outcome="success"}`.
    let success = metrics
        .webrtc_dtls_handshakes
        .get_or_create(&WebRtcDtlsOutcomeLabel {
            outcome: "success".into(),
        })
        .get();
    assert_eq!(
        success, 2,
        "each successful handshake must bump the counter exactly once"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fingerprint_mismatch_counts_against_the_fingerprint_bucket() {
    // The server advertises cert A but the client's
    // peer_fingerprint says it expects cert B. The handshake
    // completes the transport phase but rejects at the
    // fingerprint-verification gate — the outcome metric must
    // land on `"fingerprint_mismatch"`.
    let metrics = {
        let mut registry = prometheus_client::registry::Registry::default();
        Metrics::register(&mut registry)
    };
    let fabric_client = Arc::new(UdpMediaFabric::new().with_metrics(Arc::clone(&metrics)));
    let fabric_server = Arc::new(UdpMediaFabric::new().with_metrics(Arc::clone(&metrics)));

    let client_endpoint = fabric_client
        .allocate(IpAddr::from([127, 0, 0, 1]))
        .await
        .expect("client endpoint");
    let server_endpoint = fabric_server
        .allocate(IpAddr::from([127, 0, 0, 1]))
        .await
        .expect("server endpoint");
    let client_addr = client_endpoint.local_addr();
    let server_addr = server_endpoint.local_addr();

    let client_cert = SelfSignedCert::generate("client").unwrap();
    let server_cert = SelfSignedCert::generate("server").unwrap();
    let decoy_cert = SelfSignedCert::generate("decoy").unwrap();

    let client_cfg = DtlsLegConfig {
        local_cert: client_cert.clone(),
        role: DtlsRole::Client,
        // Wrong fingerprint — advertises decoy instead of server.
        peer_fingerprint: Fingerprint {
            algorithm: "sha-256".into(),
            value: decoy_cert.sha256_fingerprint.clone(),
        },
    };
    let server_cfg = DtlsLegConfig {
        local_cert: server_cert.clone(),
        role: DtlsRole::Server,
        peer_fingerprint: Fingerprint {
            algorithm: "sha-256".into(),
            value: client_cert.sha256_fingerprint.clone(),
        },
    };

    let client_id = client_endpoint.id();
    let server_id = server_endpoint.id();
    let fab_c = Arc::clone(&fabric_client);
    let fab_s = Arc::clone(&fabric_server);
    let client_task = tokio::spawn(async move {
        fab_c
            .run_dtls_handshake(client_id, server_addr, client_cfg)
            .await
    });
    let server_task = tokio::spawn(async move {
        fab_s
            .run_dtls_handshake(server_id, client_addr, server_cfg)
            .await
    });

    // Bounded wait — handshake will either complete + reject,
    // or the transport-level retransmit storm will stall. 10 s
    // gives comfortable headroom even on busy CI.
    let timeout = std::time::Duration::from_secs(10);
    let (client_res, server_res) = tokio::time::timeout(timeout, async move {
        tokio::try_join!(client_task, server_task)
    })
    .await
    .expect("fingerprint mismatch handshake should not hang forever")
    .expect("join");

    // The client's side sees the mismatch and bumps
    // `fingerprint_mismatch`.
    assert!(client_res.is_err(), "client must reject decoy fingerprint");
    // The server may complete or land in `other` — the test
    // only pins the side-we-control side of the metric.
    let _ = server_res;
    let mismatched = metrics
        .webrtc_dtls_handshakes
        .get_or_create(&WebRtcDtlsOutcomeLabel {
            outcome: "fingerprint_mismatch".into(),
        })
        .get();
    assert!(
        mismatched >= 1,
        "fingerprint_mismatch counter should bump on rejection, got {mismatched}"
    );
}
