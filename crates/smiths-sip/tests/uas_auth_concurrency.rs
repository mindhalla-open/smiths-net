//! A slow credential store must not stall unrelated calls: the digest
//! lookup runs off the ingress loop, while per-`Call-ID` ordering
//! keeps the authenticated INVITE's own dialog consistent.

mod uas_common;

use std::sync::Arc;
use std::time::Duration;

use smiths_sip::auth::digest::{Algorithm, Registrar, ha1, ha2, response_no_qop};
use smiths_sip::auth::{CredentialStore, Credentials};
use tokio::net::UdpSocket;
use uas_common::*;

/// Credential store that blocks its calling thread for `delay`.
struct SlowStore {
    delay: Duration,
}

impl CredentialStore for SlowStore {
    fn lookup(&self, realm: &str, username: &str) -> Option<Credentials> {
        std::thread::sleep(self.delay);
        Some(Credentials::new(username, realm, "s3cret"))
    }
}

fn nonce_of(challenge: &str) -> String {
    let idx = challenge.find("nonce=\"").expect("nonce in challenge");
    let rest = &challenge[idx + 7..];
    rest[..rest.find('"').unwrap()].to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_credential_store_does_not_stall_other_calls() {
    let store = Arc::new(SlowStore {
        delay: Duration::from_millis(500),
    });
    let registrar = Registrar::new("smiths.test", store);
    let uas = spawn_uas(|s| s.with_registrar(registrar)).await;
    let caller = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = caller.local_addr().unwrap();
    let ruri = format!("sip:alice@{uas}");

    // 1. Unauthenticated INVITE → 401 with a fresh nonce.
    let first = Msg::new("INVITE", ca, "slow-auth@test").ruri(&ruri);
    first.send(&caller, uas).await;
    let challenge = recv_matching(&caller, Duration::from_secs(2), |m| status_of(m) >= 200).await;
    assert_eq!(status_of(&challenge), 401, "{challenge}");
    let www = header(&challenge, "WWW-Authenticate").unwrap();
    let nonce = nonce_of(&www);
    Msg::new("ACK", ca, "slow-auth@test")
        .ruri(&ruri)
        .branch(&first.branch)
        .with_to_tag(&to_tag_of(&challenge).unwrap())
        .send(&caller, uas)
        .await;

    // 2. Authenticated INVITE: the lookup now blocks for 500 ms.
    let h1 = ha1(Algorithm::Md5, "alice", "smiths.test", "s3cret");
    let h2 = ha2(Algorithm::Md5, "INVITE", &ruri);
    let response = response_no_qop(Algorithm::Md5, &h1, &nonce, &h2);
    let authorization = format!(
        "Digest username=\"alice\", realm=\"smiths.test\", nonce=\"{nonce}\", uri=\"{ruri}\", \
         response=\"{response}\", algorithm=MD5"
    );
    let second = Msg::new("INVITE", ca, "slow-auth@test")
        .ruri(&ruri)
        .cseq(2)
        .header("Authorization", &authorization);
    let sent_at = tokio::time::Instant::now();
    second.send(&caller, uas).await;

    // 3. An unrelated OPTIONS must be answered while that lookup is
    //    still sleeping.
    let other = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let oa = other.local_addr().unwrap();
    Msg::new("OPTIONS", oa, "options-meanwhile@test")
        .send(&other, uas)
        .await;
    let options_ok = recv_str_within(&other, Duration::from_secs(2)).await;
    let options_latency = sent_at.elapsed();
    assert_eq!(status_of(&options_ok), 200, "{options_ok}");
    assert!(
        options_latency < Duration::from_millis(150),
        "OPTIONS waited {options_latency:?} behind a 500 ms credential lookup"
    );

    // 4. The INVITE completes once the lookup returns.
    let ok = recv_matching(&caller, Duration::from_secs(3), |m| {
        status_of(m) >= 200 && header(m, "CSeq").as_deref() == Some("2 INVITE")
    })
    .await;
    assert_eq!(status_of(&ok), 200, "{ok}");
    assert!(
        sent_at.elapsed() >= Duration::from_millis(450),
        "the 200 cannot precede the credential lookup"
    );
    let tag = to_tag_of(&ok).unwrap();
    Msg::new("ACK", ca, "slow-auth@test")
        .ruri(&ruri)
        .with_to_tag(&tag)
        .cseq(2)
        .send(&caller, uas)
        .await;
    Msg::new("BYE", ca, "slow-auth@test")
        .ruri(&ruri)
        .with_to_tag(&tag)
        .cseq(3)
        .send(&caller, uas)
        .await;
    let bye = recv_matching(&caller, Duration::from_secs(2), |m| {
        header(m, "CSeq").as_deref() == Some("3 BYE")
    })
    .await;
    assert_eq!(status_of(&bye), 200, "{bye}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_on_one_call_stay_ordered_behind_slow_auth() {
    let store = Arc::new(SlowStore {
        delay: Duration::from_millis(300),
    });
    let registrar = Registrar::new("smiths.test", store);
    let uas = spawn_uas(|s| s.with_registrar(registrar)).await;
    let caller = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = caller.local_addr().unwrap();
    let ruri = format!("sip:alice@{uas}");

    let first = Msg::new("INVITE", ca, "ordered-auth@test").ruri(&ruri);
    first.send(&caller, uas).await;
    let challenge = recv_matching(&caller, Duration::from_secs(2), |m| status_of(m) >= 200).await;
    assert_eq!(status_of(&challenge), 401, "{challenge}");
    let nonce = nonce_of(&header(&challenge, "WWW-Authenticate").unwrap());
    Msg::new("ACK", ca, "ordered-auth@test")
        .ruri(&ruri)
        .branch(&first.branch)
        .with_to_tag(&to_tag_of(&challenge).unwrap())
        .send(&caller, uas)
        .await;

    let h1 = ha1(Algorithm::Md5, "alice", "smiths.test", "s3cret");
    let h2 = ha2(Algorithm::Md5, "INVITE", &ruri);
    let response = response_no_qop(Algorithm::Md5, &h1, &nonce, &h2);
    let authorization = format!(
        "Digest username=\"alice\", realm=\"smiths.test\", nonce=\"{nonce}\", uri=\"{ruri}\", \
         response=\"{response}\", algorithm=MD5"
    );
    let second = Msg::new("INVITE", ca, "ordered-auth@test")
        .ruri(&ruri)
        .cseq(2)
        .header("Authorization", &authorization);
    second.send(&caller, uas).await;

    // A CANCEL for that INVITE lands while its auth lookup is still
    // sleeping: it is answered 200 at once, and the INVITE — which
    // has not sent a final yet — ends in 487 rather than 200.
    tokio::time::sleep(Duration::from_millis(50)).await;
    Msg::new("CANCEL", ca, "ordered-auth@test")
        .ruri(&ruri)
        .cseq(2)
        .branch(&second.branch)
        .send(&caller, uas)
        .await;
    let cancel_ok = recv_matching(&caller, Duration::from_secs(2), |m| {
        header(m, "CSeq").as_deref() == Some("2 CANCEL")
    })
    .await;
    assert_eq!(status_of(&cancel_ok), 200, "{cancel_ok}");
    let invite_final = recv_matching(&caller, Duration::from_secs(3), |m| {
        status_of(m) >= 200 && header(m, "CSeq").as_deref() == Some("2 INVITE")
    })
    .await;
    assert_eq!(status_of(&invite_final), 487, "{invite_final}");
    assert_eq!(to_tag_of(&invite_final), to_tag_of(&cancel_ok));
    Msg::new("ACK", ca, "ordered-auth@test")
        .ruri(&ruri)
        .cseq(2)
        .branch(&second.branch)
        .with_to_tag(&to_tag_of(&invite_final).unwrap())
        .send(&caller, uas)
        .await;
}
