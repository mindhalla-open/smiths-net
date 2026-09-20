//! In-dialog re-INVITE: codec change, hold, retransmit, and `CSeq`
//! ordering (RFC 3261 §14.2 / §12.2.2).

mod uas_common;

use std::time::Duration;

use tokio::net::UdpSocket;
use uas_common::*;

const CALL: &str = "reinvite@test";

async fn call_with_pcmu_pcma(client: &UdpSocket) -> (std::net::SocketAddr, String) {
    let uas = spawn_uas(|s| s).await;
    let ca = client.local_addr().unwrap();
    let invite = Msg::new("INVITE", ca, CALL).sdp(&sdp_offer(
        40_200,
        &[(0, "PCMU/8000"), (8, "PCMA/8000")],
        "sendrecv",
    ));
    let tag = establish(client, uas, &invite).await;
    (uas, tag)
}

/// Send `reinvite`, return its 2xx (skipping stray retransmits) and
/// ACK it.
async fn reinvite_ok(client: &UdpSocket, uas: std::net::SocketAddr, reinvite: &Msg) -> String {
    reinvite.send(client, uas).await;
    let ok = recv_matching(client, Duration::from_secs(2), |m| {
        status_of(m) >= 200
            && header(m, "CSeq").as_deref() == Some(&format!("{} INVITE", reinvite.cseq))
    })
    .await;
    assert_eq!(status_of(&ok), 200, "{ok}");
    Msg::new("ACK", reinvite.via, &reinvite.call_id)
        .with_to_tag(reinvite.to_tag.as_deref().unwrap())
        .cseq(reinvite.cseq)
        .send(client, uas)
        .await;
    ok
}

#[tokio::test(flavor = "multi_thread")]
async fn reinvite_with_new_codec_gets_new_answer_and_keeps_dialog() {
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();
    let (uas, tag) = call_with_pcmu_pcma(&client).await;

    let reinvite = Msg::new("INVITE", ca, CALL)
        .with_to_tag(&tag)
        .cseq(2)
        .sdp(&pcma_offer(40_202));
    let ok = reinvite_ok(&client, uas, &reinvite).await;
    assert_eq!(to_tag_of(&ok).as_deref(), Some(tag.as_str()), "same dialog");
    assert_eq!(
        header(&ok, "Content-Type").as_deref(),
        Some("application/sdp")
    );
    let body = body_of(&ok);
    assert!(
        body.contains("PCMA/8000"),
        "answer must switch to PCMA:\n{body}"
    );
    assert!(
        !body.contains("PCMU/8000"),
        "answer must not keep PCMU:\n{body}"
    );

    // Dialog is intact: BYE → 200.
    Msg::new("BYE", ca, CALL)
        .with_to_tag(&tag)
        .cseq(3)
        .send(&client, uas)
        .await;
    let bye = recv_matching(&client, Duration::from_secs(2), |m| {
        header(m, "CSeq").as_deref() == Some("3 BYE")
    })
    .await;
    assert_eq!(status_of(&bye), 200, "{bye}");
}

#[tokio::test(flavor = "multi_thread")]
async fn reinvite_hold_mirrors_direction() {
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();
    let (uas, tag) = call_with_pcmu_pcma(&client).await;

    let hold = Msg::new("INVITE", ca, CALL)
        .with_to_tag(&tag)
        .cseq(2)
        .sdp(&sdp_offer(40_200, &[(0, "PCMU/8000")], "sendonly"));
    let ok = reinvite_ok(&client, uas, &hold).await;
    assert!(
        body_of(&ok).contains("a=recvonly"),
        "sendonly → recvonly:\n{ok}"
    );

    let mute = Msg::new("INVITE", ca, CALL)
        .with_to_tag(&tag)
        .cseq(3)
        .sdp(&sdp_offer(40_200, &[(0, "PCMU/8000")], "inactive"));
    let ok = reinvite_ok(&client, uas, &mute).await;
    assert!(
        body_of(&ok).contains("a=inactive"),
        "inactive → inactive:\n{ok}"
    );

    let resume = Msg::new("INVITE", ca, CALL)
        .with_to_tag(&tag)
        .cseq(4)
        .sdp(&sdp_offer(40_200, &[(0, "PCMU/8000")], "sendrecv"));
    let ok = reinvite_ok(&client, uas, &resume).await;
    assert!(body_of(&ok).contains("a=sendrecv"), "resume:\n{ok}");
}

#[tokio::test(flavor = "multi_thread")]
async fn reinvite_retransmit_is_not_answered_twice() {
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();
    let (uas, tag) = call_with_pcmu_pcma(&client).await;

    let reinvite = Msg::new("INVITE", ca, CALL)
        .with_to_tag(&tag)
        .cseq(2)
        .sdp(&pcma_offer(40_202));
    reinvite.send(&client, uas).await;
    let ok = recv_matching(&client, Duration::from_secs(2), |m| status_of(m) >= 200).await;
    assert_eq!(status_of(&ok), 200, "{ok}");

    // Identical bytes again (same branch + CSeq): the retransmit is
    // dropped; only the T1-driven 2xx retransmit follows at 500 ms.
    reinvite.send(&client, uas).await;
    expect_silence(&client, Duration::from_millis(250)).await;
    let retx = recv_str_within(&client, Duration::from_millis(900)).await;
    assert_eq!(retx, ok, "2xx retransmit must be byte-identical");

    Msg::new("ACK", ca, CALL)
        .with_to_tag(&tag)
        .cseq(2)
        .send(&client, uas)
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reinvite_with_lower_cseq_is_rejected_500() {
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();
    let (uas, tag) = call_with_pcmu_pcma(&client).await;

    let reinvite = Msg::new("INVITE", ca, CALL)
        .with_to_tag(&tag)
        .cseq(5)
        .sdp(&pcma_offer(40_202));
    reinvite_ok(&client, uas, &reinvite).await;

    let stale = Msg::new("INVITE", ca, CALL)
        .with_to_tag(&tag)
        .cseq(3)
        .sdp(&pcmu_offer(40_204));
    stale.send(&client, uas).await;
    let resp = recv_matching(&client, Duration::from_secs(2), |m| {
        header(m, "CSeq").as_deref() == Some("3 INVITE") && status_of(m) >= 200
    })
    .await;
    assert_eq!(status_of(&resp), 500, "{resp}");
    Msg::new("ACK", ca, CALL)
        .branch(&stale.branch)
        .with_to_tag(&tag)
        .cseq(3)
        .send(&client, uas)
        .await;

    // The dialog survives the rejected request.
    Msg::new("BYE", ca, CALL)
        .with_to_tag(&tag)
        .cseq(6)
        .send(&client, uas)
        .await;
    let bye = recv_matching(&client, Duration::from_secs(2), |m| {
        header(m, "CSeq").as_deref() == Some("6 BYE")
    })
    .await;
    assert_eq!(status_of(&bye), 200, "{bye}");
}

#[tokio::test(flavor = "multi_thread")]
async fn reinvite_without_offer_gets_offer_in_2xx() {
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();
    let (uas, tag) = call_with_pcmu_pcma(&client).await;

    let reinvite = Msg::new("INVITE", ca, CALL).with_to_tag(&tag).cseq(2);
    let ok = reinvite_ok(&client, uas, &reinvite).await;
    assert_eq!(
        header(&ok, "Content-Type").as_deref(),
        Some("application/sdp")
    );
    assert!(body_of(&ok).contains("m=audio"), "{ok}");
}

#[tokio::test(flavor = "multi_thread")]
async fn reinvite_for_unknown_dialog_is_481() {
    let uas = spawn_uas(|s| s).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();
    Msg::new("INVITE", ca, "reinvite-none@test")
        .with_to_tag("not-ours")
        .cseq(2)
        .send(&client, uas)
        .await;
    let resp = recv_str(&client).await;
    assert_eq!(status_of(&resp), 481, "{resp}");
}
