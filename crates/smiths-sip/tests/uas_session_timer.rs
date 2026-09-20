//! Dialog lifetime guards: RFC 4028 session timers, the 64·T1 ACK
//! timeout (RFC 3261 §13.3.1.4), and the absolute call-duration cap.

mod uas_common;

use std::time::Duration;

use smiths_sip::uas::SessionTimerConfig;
use tokio::net::UdpSocket;
use uas_common::*;

fn short_timer(min_se_secs: u64) -> SessionTimerConfig {
    SessionTimerConfig {
        enabled: true,
        default_expires: Duration::from_secs(30),
        min_se: Duration::from_secs(min_se_secs),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_with_supported_timer_gets_session_expires_and_require() {
    let uas = spawn_uas(|s| s).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    Msg::new("INVITE", ca, "timer-hdrs@test")
        .header("Supported", "timer")
        .header("Session-Expires", "1800")
        .send(&client, uas)
        .await;
    let ok = recv_matching(&client, Duration::from_secs(2), |m| status_of(m) >= 200).await;
    assert_eq!(status_of(&ok), 200, "{ok}");
    assert_eq!(
        header(&ok, "Session-Expires").as_deref(),
        Some("1800;refresher=uac")
    );
    assert_eq!(header(&ok, "Require").as_deref(), Some("timer"));
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_without_supported_timer_gets_no_session_expires() {
    let uas = spawn_uas(|s| s).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    Msg::new("INVITE", ca, "timer-unsupported@test")
        .send(&client, uas)
        .await;
    let ok = recv_matching(&client, Duration::from_secs(2), |m| status_of(m) >= 200).await;
    assert_eq!(status_of(&ok), 200, "{ok}");
    assert!(header(&ok, "Session-Expires").is_none(), "{ok}");
    assert!(header(&ok, "Require").is_none(), "{ok}");
}

#[tokio::test(flavor = "multi_thread")]
async fn session_expires_below_min_se_is_422() {
    let uas = spawn_uas(|s| s).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    Msg::new("INVITE", ca, "timer-422@test")
        .header("Supported", "timer")
        .header("Session-Expires", "30")
        .send(&client, uas)
        .await;
    let resp = recv_matching(&client, Duration::from_secs(2), |m| status_of(m) >= 200).await;
    assert_eq!(status_of(&resp), 422, "{resp}");
    assert_eq!(header(&resp, "Min-SE").as_deref(), Some("90"));
}

#[tokio::test(flavor = "multi_thread")]
async fn unrefreshed_session_is_ended_with_bye() {
    let uas = spawn_uas(|s| s.with_session_timer(short_timer(1))).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    // Interval 3 s → the UAS gives up at 3 − min(32, 1) = 2 s.
    let invite = Msg::new("INVITE", ca, "timer-expire@test")
        .header("Supported", "timer")
        .header("Session-Expires", "3");
    let tag = establish(&client, uas, &invite).await;

    let started = tokio::time::Instant::now();
    let bye = recv_matching(&client, Duration::from_secs(5), |m| m.starts_with("BYE ")).await;
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(1500) && waited <= Duration::from_millis(3500),
        "BYE expected around 2 s, came after {waited:?}"
    );
    assert_eq!(header(&bye, "CSeq").as_deref(), Some("1 BYE"));
    assert_eq!(to_tag_of(&bye).as_deref(), Some("bob-tag"));
    assert_eq!(from_tag_of(&bye).as_deref(), Some(tag.as_str()));
    client
        .send_to(response_for(&bye, 200, "OK", None).as_bytes(), uas)
        .await
        .unwrap();

    // The dialog is gone.
    Msg::new("BYE", ca, "timer-expire@test")
        .with_to_tag(&tag)
        .cseq(2)
        .send(&client, uas)
        .await;
    let resp = recv_matching(&client, Duration::from_secs(2), |m| {
        header(m, "CSeq").as_deref() == Some("2 BYE")
    })
    .await;
    assert_eq!(status_of(&resp), 481, "{resp}");
}

#[tokio::test(flavor = "multi_thread")]
async fn update_refresh_postpones_the_bye() {
    let uas = spawn_uas(|s| s.with_session_timer(short_timer(1))).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    let invite = Msg::new("INVITE", ca, "timer-refresh@test")
        .header("Supported", "timer")
        .header("Session-Expires", "3");
    let tag = establish(&client, uas, &invite).await;
    let started = tokio::time::Instant::now();

    // Refresh at 1 s with a bodiless UPDATE: the expiry moves to
    // ~1 + 2 = 3 s after the call started.
    tokio::time::sleep(Duration::from_secs(1)).await;
    Msg::new("UPDATE", ca, "timer-refresh@test")
        .with_to_tag(&tag)
        .cseq(2)
        .header("Supported", "timer")
        .header("Session-Expires", "3")
        .send(&client, uas)
        .await;
    let ok = recv_matching(&client, Duration::from_secs(2), |m| {
        header(m, "CSeq").as_deref() == Some("2 UPDATE")
    })
    .await;
    assert_eq!(status_of(&ok), 200, "{ok}");
    assert_eq!(
        header(&ok, "Session-Expires").as_deref(),
        Some("3;refresher=uac")
    );

    let bye = recv_matching(&client, Duration::from_secs(5), |m| m.starts_with("BYE ")).await;
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(2600),
        "refresh must postpone the BYE; it came after {waited:?}"
    );
    client
        .send_to(response_for(&bye, 200, "OK", None).as_bytes(), uas)
        .await
        .unwrap();
}

/// The UAS retransmits the 2xx until its ACK budget (64·T1 = 32 s in
/// production; shortened here) runs out, then ends the dialog with a
/// BYE. Real time: tokio's paused clock auto-advances past a socket
/// wait, so loopback I/O cannot be driven under virtual time.
#[tokio::test(flavor = "multi_thread")]
async fn missing_ack_terminates_dialog_after_the_2xx_budget() {
    let uas = spawn_uas(|s| s.with_invite_2xx_timeout(Duration::from_millis(1200))).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    Msg::new("INVITE", ca, "ack-timeout@test")
        .send(&client, uas)
        .await;
    let ok = recv_matching(&client, Duration::from_secs(2), |m| status_of(m) >= 200).await;
    assert_eq!(status_of(&ok), 200, "{ok}");
    let tag = to_tag_of(&ok).unwrap();

    // Never ACK. Retransmits fire at 0.5 s and 1.5 s of budget
    // (T1, then 2·T1); the 1.2 s budget is exhausted before the third.
    let started = tokio::time::Instant::now();
    let mut retransmits = 0usize;
    let bye = loop {
        let msg = recv_str_within(&client, Duration::from_secs(5)).await;
        if msg.starts_with("BYE ") {
            break msg;
        }
        assert_eq!(status_of(&msg), 200, "{msg}");
        retransmits += 1;
    };
    let waited = started.elapsed();
    assert_eq!(retransmits, 1, "one T1 retransmit before the budget ends");
    assert!(
        waited >= Duration::from_millis(1200) && waited < Duration::from_secs(3),
        "BYE expected once the budget lapsed, came after {waited:?}"
    );
    assert_eq!(from_tag_of(&bye).as_deref(), Some(tag.as_str()));
    client
        .send_to(response_for(&bye, 200, "OK", None).as_bytes(), uas)
        .await
        .unwrap();

    Msg::new("BYE", ca, "ack-timeout@test")
        .with_to_tag(&tag)
        .cseq(2)
        .send(&client, uas)
        .await;
    let resp = recv_matching(&client, Duration::from_secs(2), |m| {
        header(m, "CSeq").as_deref() == Some("2 BYE")
    })
    .await;
    assert_eq!(status_of(&resp), 481, "dialog must be gone: {resp}");
}

#[tokio::test(flavor = "multi_thread")]
async fn max_call_duration_ends_call_with_bye() {
    let uas = spawn_uas(|s| s.with_max_call_duration(Some(Duration::from_millis(400)))).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    let invite = Msg::new("INVITE", ca, "max-duration@test");
    let tag = establish(&client, uas, &invite).await;

    let bye = recv_matching(&client, Duration::from_secs(2), |m| m.starts_with("BYE ")).await;
    assert_eq!(from_tag_of(&bye).as_deref(), Some(tag.as_str()));
    client
        .send_to(response_for(&bye, 200, "OK", None).as_bytes(), uas)
        .await
        .unwrap();

    Msg::new("BYE", ca, "max-duration@test")
        .with_to_tag(&tag)
        .cseq(2)
        .send(&client, uas)
        .await;
    let resp = recv_matching(&client, Duration::from_secs(2), |m| {
        header(m, "CSeq").as_deref() == Some("2 BYE")
    })
    .await;
    assert_eq!(status_of(&resp), 481, "{resp}");
}

#[tokio::test(flavor = "multi_thread")]
async fn unlimited_duration_by_default_keeps_call_up() {
    let uas = spawn_uas(|s| s.with_max_call_duration(None)).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();
    let invite = Msg::new("INVITE", ca, "no-max@test");
    let _tag = establish(&client, uas, &invite).await;
    expect_silence(&client, Duration::from_millis(700)).await;
}
