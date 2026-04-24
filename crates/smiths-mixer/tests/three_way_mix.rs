//! Slice 5.5 integration test — three participants, each hears the
//! other two.
//!
//! Runs through the full `Conference` public API: three participants
//! join, each pushes a distinct constant-amplitude PCM frame every
//! tick, each drains their egress queue and asserts that:
//!
//! 1. They receive mixed output frames at roughly tick cadence.
//! 2. The received frames are the leave-one-out sum of the other two
//!    participants' inputs.
//! 3. Removing a participant mid-call stops their egress delivery but
//!    doesn't wedge the remaining participants.
//!
//! Why not SIP UAs: full SIP integration tests are costly (DNS,
//! ports, UDP timing, TLS in some CI matrices) and the per-hop
//! correctness we care about for the mixer is "given three ingress
//! streams, what comes out of egress." That's a mixer test, not a
//! SIP test; the SIP path has its own coverage elsewhere.

#![allow(clippy::similar_names)] // three-way topology has naturally similar bindings

use std::time::Duration;

use smiths_mixer::{AgcConfig, Conference, ConferenceConfig, ConferenceId, MixerConfig, VadScore};
use tokio::time::timeout;

fn cfg() -> ConferenceConfig {
    ConferenceConfig {
        mixer: MixerConfig {
            samples_per_frame: 4,
        },
        agc: AgcConfig {
            // Bypass AGC so we can assert exact sums.
            target_rms: u32::MAX,
            attack: 1.0,
            release: 1.0,
            max_gain: 1.0,
        },
        frame_interval: Duration::from_millis(10),
        vad_threshold: VadScore::DEFAULT_SPEECH,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_way_leave_one_out_mix() {
    let conf = Conference::spawn(ConferenceId(1), cfg());
    let (a, mut a_out) = conf.join().await;
    let (b, mut b_out) = conf.join().await;
    let (c, mut c_out) = conf.join().await;

    // Each participant pushes a distinguishable DC frame.
    let a_samples = vec![10_i16, 20, 30, 40];
    let b_samples = vec![100_i16, 200, 300, 400];
    let c_samples = vec![1_000_i16, 2_000, 3_000, 4_000];

    conf.push_frame(a, a_samples.clone()).await.unwrap();
    conf.push_frame(b, b_samples.clone()).await.unwrap();
    conf.push_frame(c, c_samples.clone()).await.unwrap();

    // Drain one egress frame from each participant.
    let a_frame = timeout(Duration::from_millis(200), a_out.recv())
        .await
        .expect("a egress timed out")
        .expect("a egress closed");
    let b_frame = timeout(Duration::from_millis(200), b_out.recv())
        .await
        .expect("b egress timed out")
        .expect("b egress closed");
    let c_frame = timeout(Duration::from_millis(200), c_out.recv())
        .await
        .expect("c egress timed out")
        .expect("c egress closed");

    // A hears B + C.
    assert_eq!(
        a_frame.samples,
        vec![
            b_samples[0] + c_samples[0],
            b_samples[1] + c_samples[1],
            b_samples[2] + c_samples[2],
            b_samples[3] + c_samples[3],
        ],
        "A's mix isn't B+C",
    );
    // B hears A + C.
    assert_eq!(
        b_frame.samples,
        vec![
            a_samples[0] + c_samples[0],
            a_samples[1] + c_samples[1],
            a_samples[2] + c_samples[2],
            a_samples[3] + c_samples[3],
        ],
        "B's mix isn't A+C",
    );
    // C hears A + B.
    assert_eq!(
        c_frame.samples,
        vec![
            a_samples[0] + b_samples[0],
            a_samples[1] + b_samples[1],
            a_samples[2] + b_samples[2],
            a_samples[3] + b_samples[3],
        ],
        "C's mix isn't A+B",
    );

    conf.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn participant_leave_stops_their_egress_but_keeps_room_alive() {
    let conf = Conference::spawn(ConferenceId(2), cfg());
    let (a, mut a_out) = conf.join().await;
    let (b, mut b_out) = conf.join().await;
    let (c, mut c_out) = conf.join().await;

    // Remove B mid-call.
    conf.leave(b).await.unwrap();

    // Push fresh frames from A and C.
    let a_samples = vec![5_i16, 10, 15, 20];
    let c_samples = vec![50_i16, 100, 150, 200];
    conf.push_frame(a, a_samples.clone()).await.unwrap();
    conf.push_frame(c, c_samples.clone()).await.unwrap();

    // B's egress channel was closed by leave(); drain should see
    // it close rather than deliver more frames. We tolerate at most
    // one stale frame already queued before leave() — the channel
    // must not keep producing new frames afterward.
    if let Ok(Some(_)) = timeout(Duration::from_millis(100), b_out.recv()).await {
        let second = timeout(Duration::from_millis(100), b_out.recv()).await;
        assert!(
            matches!(second, Err(_) | Ok(None)),
            "B kept receiving frames after leave()",
        );
    }

    // A and C still hear each other.
    let a_frame = timeout(Duration::from_millis(200), a_out.recv())
        .await
        .expect("a egress stalled")
        .expect("a egress closed");
    let c_frame = timeout(Duration::from_millis(200), c_out.recv())
        .await
        .expect("c egress stalled")
        .expect("c egress closed");
    assert_eq!(a_frame.samples, c_samples, "A should now hear only C");
    assert_eq!(c_frame.samples, a_samples, "C should now hear only A");

    conf.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dominant_speaker_picks_out_the_loud_participant() {
    let conf = Conference::spawn(ConferenceId(3), cfg());
    let (a, _a_out) = conf.join().await;
    let (b, _b_out) = conf.join().await;
    let (_c, _c_out) = conf.join().await;

    // A is silent, B is loud, C is silent.
    for _ in 0..20 {
        conf.push_frame(a, vec![0; 4]).await.unwrap();
        conf.push_frame(b, vec![5_000; 4]).await.unwrap();
        // Don't push for C — the mixer fills with silence on
        // underflow, which is exactly what we want to simulate a
        // muted participant.
        tokio::time::sleep(Duration::from_millis(15)).await;
    }

    let stats = conf.stats().await;
    assert_eq!(
        stats.dominant,
        Some(b),
        "B was the only talker, should be dominant speaker",
    );

    conf.shutdown().await;
}
