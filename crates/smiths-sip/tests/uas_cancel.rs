//! RFC 3261 §9.2: CANCEL against pending, answered, and unknown
//! INVITE transactions.

mod uas_common;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use smiths_core::media::{BridgeId, EndpointId, MediaEndpoint, MediaError};
use smiths_core::{BridgeLeg, MediaFabric};
use smiths_media::UdpMediaFabric;
use tokio::net::UdpSocket;
use uas_common::*;

/// Fabric whose `allocate` takes a while — long enough for a CANCEL
/// to arrive while the INVITE is still being processed.
struct SlowFabric {
    inner: UdpMediaFabric,
    delay: Duration,
    allocated: AtomicUsize,
    released: AtomicUsize,
}

#[async_trait]
impl MediaFabric for SlowFabric {
    async fn allocate(&self, bind_ip: IpAddr) -> Result<Arc<dyn MediaEndpoint>, MediaError> {
        tokio::time::sleep(self.delay).await;
        let ep = self.inner.allocate(bind_ip).await?;
        self.allocated.fetch_add(1, Ordering::SeqCst);
        Ok(ep)
    }
    async fn bridge(&self, a: BridgeLeg, b: BridgeLeg) -> Result<BridgeId, MediaError> {
        self.inner.bridge(a, b).await
    }
    async fn release_bridge(&self, id: BridgeId) {
        self.inner.release_bridge(id).await;
    }
    async fn release_endpoint(&self, id: EndpointId) {
        self.released.fetch_add(1, Ordering::SeqCst);
        self.inner.release_endpoint(id).await;
    }
    async fn send_packet(
        &self,
        src: EndpointId,
        dest: SocketAddr,
        bytes: &[u8],
    ) -> Result<(), MediaError> {
        self.inner.send_packet(src, dest, bytes).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_of_pending_invite_gets_200_then_487_and_releases_media() {
    let fabric = Arc::new(SlowFabric {
        inner: UdpMediaFabric::new(),
        delay: Duration::from_millis(400),
        allocated: AtomicUsize::new(0),
        released: AtomicUsize::new(0),
    });
    let uas = spawn_uas_with(Arc::clone(&fabric) as Arc<dyn MediaFabric>, |s| s).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    let invite = Msg::new("INVITE", ca, "cancel-pending@test").sdp(&pcmu_offer(40_100));
    invite.send(&client, uas).await;
    let trying = recv_str(&client).await;
    assert_eq!(status_of(&trying), 100, "{trying}");

    // CANCEL shares the INVITE's branch and CSeq number (§9.1).
    Msg::new("CANCEL", ca, "cancel-pending@test")
        .branch(&invite.branch)
        .send(&client, uas)
        .await;

    let cancel_ok = recv_str(&client).await;
    assert_eq!(status_of(&cancel_ok), 200, "{cancel_ok}");
    assert_eq!(header(&cancel_ok, "CSeq").as_deref(), Some("1 CANCEL"));
    let cancel_tag = to_tag_of(&cancel_ok).expect("200 to CANCEL carries a To-tag");

    let terminated = recv_str(&client).await;
    assert_eq!(status_of(&terminated), 487, "{terminated}");
    assert_eq!(header(&terminated, "CSeq").as_deref(), Some("1 INVITE"));
    assert_eq!(
        to_tag_of(&terminated).as_deref(),
        Some(cancel_tag.as_str()),
        "487 and the CANCEL's 200 must share a To-tag"
    );

    // ACK the non-2xx final (same branch, §17.1.1.3).
    Msg::new("ACK", ca, "cancel-pending@test")
        .branch(&invite.branch)
        .with_to_tag(&cancel_tag)
        .send(&client, uas)
        .await;

    // No dialog was created: a BYE for it is 481.
    Msg::new("BYE", ca, "cancel-pending@test")
        .with_to_tag(&cancel_tag)
        .cseq(2)
        .send(&client, uas)
        .await;
    let bye_resp = recv_str(&client).await;
    assert_eq!(status_of(&bye_resp), 481, "{bye_resp}");

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fabric.allocated.load(Ordering::SeqCst), 1);
    assert_eq!(
        fabric.released.load(Ordering::SeqCst),
        1,
        "the cancelled INVITE's endpoint must be released"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_after_2xx_is_200_and_leaves_dialog_intact() {
    let uas = spawn_uas(|s| s).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    let invite = Msg::new("INVITE", ca, "cancel-late@test");
    invite.send(&client, uas).await;
    let ok = recv_matching(&client, Duration::from_secs(2), |m| status_of(m) >= 200).await;
    assert_eq!(status_of(&ok), 200, "{ok}");
    let tag = to_tag_of(&ok).unwrap();

    Msg::new("CANCEL", ca, "cancel-late@test")
        .branch(&invite.branch)
        .send(&client, uas)
        .await;
    let cancel_resp = recv_matching(&client, Duration::from_secs(2), |m| {
        header(m, "CSeq").as_deref() == Some("1 CANCEL")
    })
    .await;
    assert_eq!(status_of(&cancel_resp), 200, "{cancel_resp}");
    assert_eq!(to_tag_of(&cancel_resp).as_deref(), Some(tag.as_str()));

    // The dialog is unaffected: ACK then BYE → 200.
    Msg::new("ACK", ca, "cancel-late@test")
        .with_to_tag(&tag)
        .send(&client, uas)
        .await;
    Msg::new("BYE", ca, "cancel-late@test")
        .with_to_tag(&tag)
        .cseq(2)
        .send(&client, uas)
        .await;
    let bye_resp = recv_matching(&client, Duration::from_secs(2), |m| {
        header(m, "CSeq").as_deref() == Some("2 BYE")
    })
    .await;
    assert_eq!(status_of(&bye_resp), 200, "{bye_resp}");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_on_parked_early_rendezvous_leg_has_no_effect() {
    let uas = spawn_uas(|s| s).await;
    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (aa, ba) = (a.local_addr().unwrap(), b.local_addr().unwrap());

    // Leg A parks on the room and is answered but never ACKs (Early).
    let invite_a = Msg::new("INVITE", aa, "cancel-park-a@test")
        .room("room-cancel")
        .with_from_tag("a-tag")
        .sdp(&pcmu_offer(40_110));
    invite_a.send(&a, uas).await;
    let ok_a = recv_matching(&a, Duration::from_secs(2), |m| status_of(m) >= 200).await;
    assert_eq!(status_of(&ok_a), 200, "{ok_a}");
    let tag_a = to_tag_of(&ok_a).unwrap();

    Msg::new("CANCEL", aa, "cancel-park-a@test")
        .room("room-cancel")
        .with_from_tag("a-tag")
        .branch(&invite_a.branch)
        .send(&a, uas)
        .await;
    let cancel_resp = recv_matching(&a, Duration::from_secs(2), |m| {
        header(m, "CSeq").as_deref() == Some("1 CANCEL")
    })
    .await;
    assert_eq!(status_of(&cancel_resp), 200, "{cancel_resp}");

    // Leg A is still parked: leg B pairs with it and gets a 200.
    let invite_b = Msg::new("INVITE", ba, "cancel-park-b@test")
        .room("room-cancel")
        .with_from_tag("b-tag")
        .sdp(&pcmu_offer(40_112));
    let tag_b = establish(&b, uas, &invite_b).await;

    // Leg A's dialog still exists: its BYE is answered 200 …
    Msg::new("ACK", aa, "cancel-park-a@test")
        .room("room-cancel")
        .with_from_tag("a-tag")
        .with_to_tag(&tag_a)
        .send(&a, uas)
        .await;
    Msg::new("BYE", aa, "cancel-park-a@test")
        .room("room-cancel")
        .with_from_tag("a-tag")
        .with_to_tag(&tag_a)
        .cseq(2)
        .send(&a, uas)
        .await;
    let bye_a = recv_matching(&a, Duration::from_secs(2), |m| {
        header(m, "CSeq").as_deref() == Some("2 BYE")
    })
    .await;
    assert_eq!(status_of(&bye_a), 200, "{bye_a}");

    // … and, being bridged, ends leg B too (engine BYE toward B).
    let bye_to_b = recv_matching(&b, Duration::from_secs(2), |m| m.starts_with("BYE ")).await;
    assert_eq!(to_tag_of(&bye_to_b).as_deref(), Some("b-tag"));
    assert_eq!(from_tag_of(&bye_to_b).as_deref(), Some(tag_b.as_str()));
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_without_matching_transaction_is_481() {
    let uas = spawn_uas(|s| s).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    Msg::new("CANCEL", ca, "cancel-none@test")
        .send(&client, uas)
        .await;
    let resp = recv_str(&client).await;
    assert_eq!(status_of(&resp), 481, "{resp}");
    assert_eq!(header(&resp, "CSeq").as_deref(), Some("1 CANCEL"));
}
