//! Full-stack scenario: two fake UAs rendezvous through a real UAS,
//! RTP crosses the bridge, the metrics registry reflects it, and a
//! drain refuses the next INVITE while the live call keeps flowing.
//!
//! Every other test in `smiths-testkit` exercises one subsystem; this
//! one stitches signaling, media, observability and shutdown
//! together and asserts at the seams.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use prometheus_client::registry::Registry;
use smiths_core::{Drain, EventBus, MediaFabric, Metrics, SdpNegotiator};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Negotiator;
use smiths_sip::Transport as _;
use smiths_sip::{UasServer, UdpTransport};
use smiths_testkit::FakeUac;
use smiths_testkit::rtp::RtpPacket;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const RENDEZVOUS: &str = "full-stack-room";
const FRAMES: usize = 25;

struct Engine {
    addr: SocketAddr,
    cancel: CancellationToken,
    task: JoinHandle<()>,
    drain: Drain,
    metrics: Arc<Metrics>,
    registry: Arc<Mutex<Registry>>,
}

async fn start_engine() -> Engine {
    let t = UdpTransport::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = t.local_addr().unwrap();
    let transport = Arc::new(t);
    let bus = EventBus::new(32);
    let cancel = CancellationToken::new();
    let (tx, rx) = mpsc::channel(64);
    transport.spawn_reader(tx, cancel.clone());

    let mut registry = Registry::default();
    let metrics = Metrics::register(&mut registry);
    let drain = Drain::new();
    let fabric: Arc<dyn MediaFabric> =
        Arc::new(UdpMediaFabric::new().with_metrics(Arc::clone(&metrics)));
    let negotiator: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(addr.ip()));
    let server = UasServer::new(Arc::clone(&transport), bus, fabric, negotiator)
        .unwrap()
        .with_metrics(Arc::clone(&metrics))
        .with_drain(drain.clone());
    let task = tokio::spawn(server.run(rx, cancel.clone()));
    Engine {
        addr,
        cancel,
        task,
        drain,
        metrics,
        registry: Arc::new(Mutex::new(registry)),
    }
}

async fn scrape(registry: &Arc<Mutex<Registry>>) -> String {
    let mut out = String::new();
    prometheus_client::encoding::text::encode(&mut out, &*registry.lock().await).unwrap();
    out
}

/// Value of a single-sample metric line (`name{labels} value`).
fn sample(scrape: &str, prefix: &str) -> Option<f64> {
    scrape
        .lines()
        .find(|l| l.starts_with(prefix))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
}

#[tokio::test(flavor = "multi_thread")]
async fn full_stack_call_metrics_and_drain() {
    let engine = start_engine().await;

    // 1. Two UAs rendezvous; both get 200 OK with a media answer.
    let mut ua_a = FakeUac::bind(engine.addr).await.unwrap();
    let mut ua_b = FakeUac::bind(engine.addr).await.unwrap();
    let (inv_a, inv_b) = tokio::join!(ua_a.invite(RENDEZVOUS), ua_b.invite(RENDEZVOUS));
    inv_a.expect("UA-A INVITE");
    inv_b.expect("UA-B INVITE");
    let rtp_target_a = ua_a.engine_rtp.expect("UA-A media answer");
    assert_eq!(engine.metrics.dialogs_active.get(), 2);
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 2. RTP from A crosses the bridge to B.
    let collector = tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        let mut got = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while got < FRAMES && tokio::time::Instant::now() < deadline {
            match timeout(Duration::from_millis(300), ua_b.rtp.recv_from(&mut buf)).await {
                Ok(Ok((n, _))) if RtpPacket::decode(&buf[..n]).is_some() => got += 1,
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        (got, ua_b)
    });
    for i in 0..FRAMES {
        let pkt = RtpPacket {
            marker: i == 0,
            payload_type: 0,
            sequence: u16::try_from(i).unwrap(),
            timestamp: u32::try_from(i * 160).unwrap(),
            ssrc: 0x1234_5678,
            payload: vec![0xFF; 160],
        };
        ua_a.rtp.send_to(&pkt.encode(), rtp_target_a).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (received, mut ua_b) = collector.await.unwrap();
    assert!(
        received >= FRAMES - 2,
        "UA-B received {received} of {FRAMES} frames"
    );

    // 3. Metrics moved: dialogs, bridges, forwarded packets.
    let text = scrape(&engine.registry).await;
    assert_eq!(
        sample(&text, "smiths_sip_dialogs_active"),
        Some(2.0),
        "{text}"
    );
    assert_eq!(
        sample(&text, "smiths_media_bridges_active"),
        Some(1.0),
        "{text}"
    );
    let forwarded = sample(
        &text,
        "smiths_rtp_packets_forwarded_total{direction=\"a_to_b\"}",
    )
    .or_else(|| {
        sample(
            &text,
            "smiths_rtp_packets_forwarded_total{direction=\"b_to_a\"}",
        )
    })
    .expect("forwarded counter present");
    assert!(
        forwarded >= f64::from(u32::try_from(FRAMES - 2).expect("frame count fits u32")),
        "forwarded={forwarded}\n{text}"
    );
    assert!(
        text.contains("smiths_sip_requests_total{method=\"INVITE\"} 2"),
        "{text}"
    );

    // 4. Drain: a fresh INVITE is refused with 503 while the live
    //    dialogs stay up and still BYE cleanly.
    engine.drain.start();
    let mut ua_c = FakeUac::bind(engine.addr).await.unwrap();
    let status = ua_c
        .invite_expect_rejection("another-room")
        .await
        .expect("rejection status line");
    assert!(status.starts_with("SIP/2.0 503"), "got: {status}");
    assert_eq!(
        engine.metrics.dialogs_active.get(),
        2,
        "drain must not drop live dialogs"
    );

    ua_a.bye(RENDEZVOUS).await.expect("UA-A BYE");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while engine.metrics.dialogs_active.get() != 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        engine.metrics.dialogs_active.get(),
        0,
        "B2BUA BYE must end both legs"
    );
    let _ = ua_b.bye(RENDEZVOUS).await;

    engine.cancel.cancel();
    timeout(Duration::from_secs(2), engine.task)
        .await
        .expect("UAS must stop on cancel")
        .unwrap();
}
