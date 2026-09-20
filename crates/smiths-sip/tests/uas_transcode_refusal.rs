//! A rendezvous codec mismatch that cannot be transcoded is refused
//! with a final failure instead of a silent, inaudible passthrough
//! bridge; the waiting leg stays parked for the next caller.

mod uas_common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use smiths_core::media::MediaSession;
use smiths_core::{BridgeLeg, MediaError, NegotiatedCodec};
use smiths_sip::TranscodeOrchestrator;
use tokio::net::UdpSocket;
use uas_common::*;

/// Orchestrator whose CPU budget is always exhausted.
struct RefusingOrchestrator;

#[async_trait]
impl TranscodeOrchestrator for RefusingOrchestrator {
    async fn try_orchestrate(
        &self,
        _leg_a: BridgeLeg,
        _codec_a: NegotiatedCodec,
        _leg_b: BridgeLeg,
        _codec_b: NegotiatedCodec,
    ) -> Result<Option<Arc<dyn MediaSession>>, MediaError> {
        Ok(None)
    }
}

async fn invite_final(sock: &UdpSocket, uas: std::net::SocketAddr, invite: &Msg) -> String {
    invite.send(sock, uas).await;
    recv_matching(sock, Duration::from_secs(2), |m| status_of(m) >= 200).await
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_refusal_is_503_and_first_leg_stays_parked() {
    let uas = spawn_uas(|s| s.with_transcode_orchestrator(Arc::new(RefusingOrchestrator))).await;
    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    let invite_a = Msg::new("INVITE", a.local_addr().unwrap(), "xcode-a@test")
        .room("room-xcode")
        .with_from_tag("a-tag")
        .sdp(&pcmu_offer(40_400));
    let _tag_a = establish(&a, uas, &invite_a).await;

    let invite_b = Msg::new("INVITE", b.local_addr().unwrap(), "xcode-b@test")
        .room("room-xcode")
        .with_from_tag("b-tag")
        .sdp(&opus_offer(40_402));
    let refused = invite_final(&b, uas, &invite_b).await;
    assert_eq!(status_of(&refused), 503, "{refused}");
    let warning = header(&refused, "Warning").unwrap_or_default();
    assert!(warning.starts_with("370 "), "Warning: {warning}");
    assert!(header(&refused, "Retry-After").is_some(), "{refused}");
    Msg::new("ACK", b.local_addr().unwrap(), "xcode-b@test")
        .room("room-xcode")
        .with_from_tag("b-tag")
        .branch(&invite_b.branch)
        .with_to_tag(&to_tag_of(&refused).unwrap())
        .send(&b, uas)
        .await;

    // Leg A is still parked: a PCMU caller pairs with it.
    let invite_c = Msg::new("INVITE", c.local_addr().unwrap(), "xcode-c@test")
        .room("room-xcode")
        .with_from_tag("c-tag")
        .sdp(&pcmu_offer(40_404));
    let ok = invite_final(&c, uas, &invite_c).await;
    assert_eq!(status_of(&ok), 200, "{ok}");
    assert!(body_of(&ok).contains("PCMU/8000"), "{ok}");
}

#[tokio::test(flavor = "multi_thread")]
async fn codec_mismatch_without_transcoder_is_488() {
    let uas = spawn_uas(|s| s).await;
    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    let invite_a = Msg::new("INVITE", a.local_addr().unwrap(), "nox-a@test")
        .room("room-nox")
        .with_from_tag("a-tag")
        .sdp(&pcmu_offer(40_410));
    let _tag_a = establish(&a, uas, &invite_a).await;

    let invite_b = Msg::new("INVITE", b.local_addr().unwrap(), "nox-b@test")
        .room("room-nox")
        .with_from_tag("b-tag")
        .sdp(&opus_offer(40_412));
    let refused = invite_final(&b, uas, &invite_b).await;
    assert_eq!(status_of(&refused), 488, "{refused}");
    let warning = header(&refused, "Warning").unwrap_or_default();
    assert!(warning.starts_with("399 "), "Warning: {warning}");
}
