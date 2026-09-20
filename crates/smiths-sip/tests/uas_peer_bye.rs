//! Engine-originated BYE toward the surviving leg of a bridge: built
//! from the dialog's remote target, route set, real `From`/`To` and
//! local `CSeq` (RFC 3261 §12.2.1.1), and correlated with its response
//! on branch + `CSeq` method (§17.1.3).

mod uas_common;

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::UdpSocket;
use uas_common::*;

struct Bridged {
    uas: SocketAddr,
    a: UdpSocket,
    b: UdpSocket,
    tag_a: String,
    tag_b: String,
}

/// Two legs on one room; leg A registers a Contact + Record-Route.
async fn bridged_pair(room: &str) -> Bridged {
    let uas = spawn_uas(|s| s).await;
    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (aa, ba) = (a.local_addr().unwrap(), b.local_addr().unwrap());

    let invite_a = Msg::new("INVITE", aa, &format!("{room}-a@test"))
        .room(room)
        .with_from_uri("sip:alice@example.net")
        .with_from_tag("a-tag")
        .header(
            "Contact",
            &format!("<sip:alice-contact@{aa};transport=udp>"),
        )
        .header(
            "Record-Route",
            "<sip:proxy1.example;lr>, <sip:proxy2.example;lr>",
        )
        .sdp(&pcmu_offer(40_300));
    let tag_a = establish(&a, uas, &invite_a).await;

    let invite_b = Msg::new("INVITE", ba, &format!("{room}-b@test"))
        .room(room)
        .with_from_tag("b-tag")
        .sdp(&pcmu_offer(40_302));
    let tag_b = establish(&b, uas, &invite_b).await;
    Bridged {
        uas,
        a,
        b,
        tag_a,
        tag_b,
    }
}

async fn hang_up_b(br: &Bridged, room: &str) {
    Msg::new("BYE", br.b.local_addr().unwrap(), &format!("{room}-b@test"))
        .room(room)
        .with_from_tag("b-tag")
        .with_to_tag(&br.tag_b)
        .cseq(2)
        .send(&br.b, br.uas)
        .await;
    let ok = recv_matching(&br.b, Duration::from_secs(2), |m| {
        header(m, "CSeq").as_deref() == Some("2 BYE")
    })
    .await;
    assert_eq!(status_of(&ok), 200, "{ok}");
}

#[tokio::test(flavor = "multi_thread")]
async fn peer_bye_uses_remote_target_route_set_and_dialog_identity() {
    let room = "room-peerbye";
    let br = bridged_pair(room).await;
    let aa = br.a.local_addr().unwrap();
    hang_up_b(&br, room).await;

    let bye = recv_matching(&br.a, Duration::from_secs(2), |m| m.starts_with("BYE ")).await;
    assert!(
        bye.starts_with(&format!(
            "BYE sip:alice-contact@{aa};transport=udp SIP/2.0\r\n"
        )),
        "Request-URI must be leg A's Contact:\n{bye}"
    );
    assert_eq!(
        headers(&bye, "Route"),
        vec![
            "<sip:proxy1.example;lr>".to_owned(),
            "<sip:proxy2.example;lr>".to_owned()
        ],
        "route set copied in order:\n{bye}"
    );
    let via = header(&bye, "Via").unwrap();
    assert!(via.starts_with("SIP/2.0/UDP "), "{via}");
    assert_eq!(
        header(&bye, "Call-ID").as_deref(),
        Some(format!("{room}-a@test").as_str())
    );
    assert_eq!(header(&bye, "CSeq").as_deref(), Some("1 BYE"));
    let from = header(&bye, "From").unwrap();
    assert!(
        from.starts_with(&format!("<sip:{room}@127.0.0.1>")),
        "From must be the dialog's local URI (leg A's To):\n{from}"
    );
    assert_eq!(from_tag_of(&bye).as_deref(), Some(br.tag_a.as_str()));
    let to = header(&bye, "To").unwrap();
    assert!(
        to.starts_with("<sip:alice@example.net>"),
        "To must be leg A's From URI:\n{to}"
    );
    assert_eq!(to_tag_of(&bye).as_deref(), Some("a-tag"));

    // A matching 200 (same branch, `CSeq: 1 BYE`) stops retransmission.
    br.a.send_to(response_for(&bye, 200, "OK", None).as_bytes(), br.uas)
        .await
        .unwrap();
    expect_silence(&br.a, Duration::from_millis(1200)).await;

    // Leg A's dialog is gone on the engine side.
    Msg::new("BYE", aa, &format!("{room}-a@test"))
        .room(room)
        .with_from_tag("a-tag")
        .with_to_tag(&br.tag_a)
        .cseq(2)
        .send(&br.a, br.uas)
        .await;
    let resp = recv_matching(&br.a, Duration::from_secs(2), |m| {
        header(m, "CSeq").as_deref() == Some("2 BYE")
    })
    .await;
    assert_eq!(status_of(&resp), 481, "{resp}");
}

#[tokio::test(flavor = "multi_thread")]
async fn peer_bye_response_with_wrong_cseq_method_is_not_matched() {
    let room = "room-peerbye-cseq";
    let br = bridged_pair(room).await;
    hang_up_b(&br, room).await;

    let bye = recv_matching(&br.a, Duration::from_secs(2), |m| m.starts_with("BYE ")).await;
    let branch = branch_of(&bye).unwrap();

    // Same branch, but the CSeq names INVITE: §17.1.3 says this is
    // not a response to the BYE transaction, so timer E keeps
    // retransmitting the BYE.
    br.a.send_to(
        response_for(&bye, 200, "OK", Some("CSeq: 1 INVITE")).as_bytes(),
        br.uas,
    )
    .await
    .unwrap();
    let retx = recv_matching(&br.a, Duration::from_millis(1500), |m| {
        m.starts_with("BYE ")
    })
    .await;
    assert_eq!(branch_of(&retx).as_deref(), Some(branch.as_str()));

    // The real answer stops it.
    br.a.send_to(response_for(&retx, 200, "OK", None).as_bytes(), br.uas)
        .await
        .unwrap();
    expect_silence(&br.a, Duration::from_millis(1200)).await;
}
