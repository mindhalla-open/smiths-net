//! End-to-end: engine-side UAC places a call to a `FakeUas`, the

#![allow(clippy::format_push_string, clippy::uninlined_format_args)]
//!
//! fake answers 200 OK with an SDP answer, UAC ACKs, and a later
//! `hangup` sends BYE which the fake 200s. Verifies
//! `DialogCreated` + `DialogTerminated` hit the bus with the right
//! `call_id`.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use smiths_core::media::MediaFabric;
use smiths_core::{Event, EventBus, Metrics, SdpNegotiator, SipEvent};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Negotiator;
use smiths_sip::{ResponseRouter, Transport as _, UasServer, UdpTransport};
use smiths_testkit::FakeUas;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

/// Parse one header value from a raw SIP request.
fn header<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let lower_name = name.to_ascii_lowercase();
    for line in text.split("\r\n") {
        let Some((n, v)) = line.split_once(':') else {
            continue;
        };
        if n.trim().eq_ignore_ascii_case(&lower_name) {
            return Some(v.trim());
        }
    }
    None
}

/// Build a minimal 200 OK response for `req` (which we received over
/// UDP). `body` is either an SDP answer or empty.
fn build_200(req: &str, local_tag: &str, body: &str) -> Bytes {
    let via = header(req, "via").unwrap_or("").to_owned();
    let from = header(req, "from").unwrap_or("").to_owned();
    let raw_to = header(req, "to").unwrap_or("").to_owned();
    let to_with_tag = if raw_to.contains(";tag=") {
        raw_to
    } else {
        format!("{raw_to};tag={local_tag}")
    };
    let call_id = header(req, "call-id").unwrap_or("").to_owned();
    let cseq = header(req, "cseq").unwrap_or("").to_owned();
    let content_type = if body.is_empty() {
        String::new()
    } else {
        "Content-Type: application/sdp\r\n".to_owned()
    };
    let mut out = String::new();
    out.push_str("SIP/2.0 200 OK\r\n");
    out.push_str(&format!("Via: {via}\r\n"));
    out.push_str(&format!("From: {from}\r\n"));
    out.push_str(&format!("To: {to_with_tag}\r\n"));
    out.push_str(&format!("Call-ID: {call_id}\r\n"));
    out.push_str(&format!("CSeq: {cseq}\r\n"));
    out.push_str(&content_type);
    out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    out.push_str(body);
    Bytes::from(out.into_bytes())
}

#[tokio::test(flavor = "multi_thread")]
async fn uac_places_call_and_hangs_up_against_fake_uas() {
    // --- Fake remote UAS ---
    let fake = Arc::new(FakeUas::bind().await.unwrap());
    let fake_addr = fake.local_addr().unwrap();
    let fake_rtp_port: u16 = 55_000; // any free-ish high port; the test never binds it

    // --- Engine-side transport + UAS + UAC ---
    let engine_transport = UdpTransport::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let engine_addr = engine_transport.local_addr().unwrap();
    let engine_transport = Arc::new(engine_transport);

    let bus = EventBus::new(64);
    let cancel = CancellationToken::new();
    let (tx, rx) = mpsc::channel(64);
    engine_transport.spawn_reader(tx, cancel.clone());

    let router = Arc::new(ResponseRouter::new());
    let media_fabric: Arc<dyn MediaFabric> = Arc::new(UdpMediaFabric::new());
    let negotiator: Arc<dyn SdpNegotiator> =
        Arc::new(Negotiator::with_default_codecs(engine_addr.ip()));

    let server = UasServer::new(
        Arc::clone(&engine_transport),
        bus.clone(),
        Arc::clone(&media_fabric),
        Arc::clone(&negotiator),
    )
    .unwrap()
    .with_response_router(Arc::clone(&router));
    tokio::spawn(server.run(rx, cancel.clone()));

    let uac = Arc::new(smiths_sip::UacClient::new(
        Arc::clone(&engine_transport),
        bus.clone(),
        Arc::clone(&media_fabric),
        negotiator,
        Arc::clone(&router),
        engine_addr,
        Metrics::noop(),
    ));

    // Subscribe to the bus before we fire anything.
    let mut bus_rx = bus.subscribe();

    // --- Fake UAS responder task ---
    // Receives the INVITE, replies 200 + SDP, then waits for BYE and
    // replies 200 to that too. Crude but sufficient.
    let fake_for_task = Arc::clone(&fake);
    let responder = tokio::spawn(async move {
        // INVITE
        let captured = fake_for_task.recv_request().await.unwrap();
        let req_text = captured.as_str().into_owned();
        assert!(
            req_text.starts_with("INVITE"),
            "expected INVITE, got: {}",
            req_text.lines().next().unwrap_or("")
        );
        let answer_sdp = format!(
            "v=0\r\n\
             o=remote 1 1 IN IP4 127.0.0.1\r\n\
             s=-\r\n\
             c=IN IP4 127.0.0.1\r\n\
             t=0 0\r\n\
             m=audio {fake_rtp_port} RTP/AVP 0\r\n\
             a=rtpmap:0 PCMU/8000\r\n\
             a=sendrecv\r\n",
        );
        let resp = build_200(&req_text, "remote-tag-42", &answer_sdp);
        fake_for_task.send_raw(&resp, captured.peer).await.unwrap();

        // ACK (silently consumed — no response to send)
        let ack = fake_for_task.recv_request().await.unwrap();
        assert!(ack.as_str().starts_with("ACK"), "expected ACK");

        // BYE → 200 OK
        let bye = fake_for_task.recv_request().await.unwrap();
        let bye_text = bye.as_str().into_owned();
        assert!(bye_text.starts_with("BYE"), "expected BYE");
        let bye_200 = build_200(&bye_text, "remote-tag-42", "");
        fake_for_task.send_raw(&bye_200, bye.peer).await.unwrap();
    });

    // --- Place the call via the UAC ---
    let target = format!("sip:echo@{fake_addr}");
    let call_id = uac.place_call(&target).await.expect("place_call");

    // DialogCreated must fire with the right call_id + media info.
    let created = timeout(Duration::from_secs(2), bus_rx.recv())
        .await
        .unwrap()
        .unwrap();
    match created {
        Event::Sip(SipEvent::DialogCreated {
            call_id: cid,
            media_endpoint,
            remote_rtp,
            ..
        }) => {
            assert_eq!(cid, call_id);
            assert!(media_endpoint.is_some(), "UAC should have allocated media");
            let rtp = remote_rtp.expect("peer advertised RTP");
            assert_eq!(rtp.port(), fake_rtp_port);
        }
        other => panic!("expected DialogCreated, got {other:?}"),
    }

    // Hang up.
    uac.hangup(&call_id).await.expect("hangup");
    let terminated = timeout(Duration::from_secs(2), bus_rx.recv())
        .await
        .unwrap()
        .unwrap();
    match terminated {
        Event::Sip(SipEvent::DialogTerminated { call_id: cid }) => assert_eq!(cid, call_id),
        other => panic!("expected DialogTerminated, got {other:?}"),
    }

    responder.await.unwrap();
    cancel.cancel();
}
