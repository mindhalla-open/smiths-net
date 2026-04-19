//! End-to-end: agent calls `speak(call_id, plugin, text)`, engine
//! synthesizes via `ai-tts-mock`, chunks the PCM into RTP, and the
//! test's fake UA receives μ-law frames on its RTP socket.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use smiths_core::ai::AiRegistry;
use smiths_core::{Config, EventBus, Metrics, RateLimitConfig};
use smiths_core::{MediaFabric, SdpNegotiator};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Negotiator;
use smiths_sip::{Transport as _, UasServer, UdpTransport};
use smiths_testkit::FakeUac;
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;

const RENDEZVOUS: &str = "speak-test";

fn tts_mock_dir() -> PathBuf {
    // crate root → workspace root → plugins/examples/ai-tts-mock
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("plugins/examples/ai-tts-mock"))
        .expect("workspace root")
}

/// Spin up the engine: SIP UAS + media fabric + plugin registry +
/// MCP tool registry. Returns the engine's SIP address, its
/// `ToolContext`, and a cancellation token.
#[allow(clippy::type_complexity)]
async fn spawn_engine() -> (
    SocketAddr,
    smiths_mcp::ToolContext,
    Arc<smiths_mcp::ToolRegistry>,
    Arc<smiths_mcp::RateLimiter>,
    Arc<Metrics>,
    CancellationToken,
    smiths_plugin::AiRegistry,
) {
    let transport = UdpTransport::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let local = transport.local_addr().unwrap();
    let transport = Arc::new(transport);
    let bus = EventBus::new(64);
    let cancel = CancellationToken::new();
    let (tx, rx) = mpsc::channel(128);
    transport.spawn_reader(tx, cancel.clone());

    let media_fabric: Arc<dyn MediaFabric> = Arc::new(UdpMediaFabric::new());
    let negotiator: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(local.ip()));
    let server = UasServer::new(
        Arc::clone(&transport),
        bus.clone(),
        Arc::clone(&media_fabric),
        negotiator,
    )
    .unwrap();
    tokio::spawn(server.run(rx, cancel.clone()));

    // Load the real ai-tts-mock plugin from disk.
    let ai_registry = smiths_plugin::AiRegistry::new();
    // load_plugins scans a DIRECTORY for subdirectories; feed it the
    // parent so only ai-tts-mock matches.
    let examples = tts_mock_dir().parent().unwrap().to_path_buf();
    // Filter: copy just ai-tts-mock into a fresh tempdir so other
    // example plugins don't get loaded and slow the test.
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("ai-tts-mock");
    std::fs::create_dir_all(&dest).unwrap();
    for entry in std::fs::read_dir(tts_mock_dir()).unwrap() {
        let e = entry.unwrap();
        let target = dest.join(e.file_name());
        std::fs::copy(e.path(), &target).unwrap();
    }
    // Preserve exec bit on main.py.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let main = dest.join("main.py");
        let mut p = std::fs::metadata(&main).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&main, p).unwrap();
    }
    let _ = examples;
    let report = smiths_plugin::load_plugins(tmp.path(), &ai_registry)
        .await
        .unwrap();
    assert!(
        report.loaded.iter().any(|n| n == "ai-tts-mock"),
        "ai-tts-mock failed to load: {:?}",
        report.failed
    );
    // Keep the tempdir alive for the life of the test.
    std::mem::forget(tmp);

    let (state, _task) = smiths_mcp::ControlState::spawn(&bus, cancel.clone());
    let ai_registry_dyn: Arc<dyn AiRegistry> = Arc::new(ai_registry.clone());
    let ctx = smiths_mcp::ToolContext::new(
        state,
        ai_registry_dyn,
        Arc::new(Config::default()),
        Arc::clone(&media_fabric),
    );
    let tools = Arc::new(smiths_mcp::tools::builtin_registry());
    let rl = Arc::new(smiths_mcp::RateLimiter::new(&RateLimitConfig::default()));
    let metrics = Metrics::noop();

    (local, ctx, tools, rl, metrics, cancel, ai_registry)
}

#[tokio::test(flavor = "multi_thread")]
async fn speak_injects_rtp_into_live_call() {
    let (engine, ctx, tools, rl, metrics, cancel, ai_registry) = spawn_engine().await;

    // UA places an INVITE; the engine establishes a dialog.
    let mut ua = FakeUac::bind(engine).await.unwrap();
    ua.invite(RENDEZVOUS).await.expect("UA INVITE");

    // Give the DialogCreated event a moment to hit ControlState.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Invoke `speak` through the shared dispatcher.
    let out = smiths_mcp::dispatch::invoke_audited(
        &tools,
        &rl,
        &metrics,
        &ctx,
        "test",
        "speak",
        json!({
            "call_id": ua.call_id.clone(),
            "plugin":  "ai-tts-mock",
            "text":    "hello world",
            "voice":   "alice",
        }),
    )
    .await
    .expect("speak call");

    let frames_sent = out["frames_sent"].as_u64().expect("frames_sent");
    assert!(frames_sent >= 1, "speak sent no frames; response: {out}");

    // UA should have received at least one RTP packet on its RTP
    // socket from the engine's media endpoint. Collect until quiet.
    let mut buf = vec![0u8; 2048];
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut received: Vec<Vec<u8>> = Vec::new();
    while Instant::now() < deadline && (received.len() as u64) < frames_sent {
        match timeout(Duration::from_millis(500), ua.rtp.recv_from(&mut buf)).await {
            Ok(Ok((n, _src))) => received.push(buf[..n].to_vec()),
            _ => break,
        }
    }
    assert!(
        !received.is_empty(),
        "UA received no RTP frames; speak reported {frames_sent}"
    );

    // Decode first RTP packet and check it's PCMU (PT=0) with non-empty payload.
    let pkt = smiths_media::RtpPacket::decode(&received[0]).expect("valid RTP");
    assert_eq!(
        pkt.payload_type, 0,
        "expected PCMU, got PT {}",
        pkt.payload_type
    );
    assert!(!pkt.payload.is_empty());

    // Verify all packets share the same SSRC (stable per-call).
    let ssrc0 = pkt.ssrc;
    for raw in &received[1..] {
        let p = smiths_media::RtpPacket::decode(raw).unwrap();
        assert_eq!(p.ssrc, ssrc0, "SSRC drifted mid-stream");
    }

    ua.bye(RENDEZVOUS).await.ok();
    ai_registry.shutdown_all().await;
    cancel.cancel();
}
