//! [`SrtpTransform`] implementation backed by `webrtc-srtp`.
//!
//! Per-direction primitive: one instance encrypts engine→peer, a
//! separate one decrypts peer→engine. The bridge composes four of
//! these per call (two legs × two directions) and drives them from
//! the forwarder hot path. Each instance also carries the SRTCP half
//! of the same master key, so the bridge's RTCP emitter and listener
//! use the same transform as the RTP forwarders on that leg.
//!
//! `webrtc-srtp`'s [`Context`] is `&mut`-stateful (per-SSRC rollover
//! counters, replay detectors), so we wrap it in a [`std::sync::Mutex`]
//! to match the `SrtpTransform` trait's `&self` signature. Contention
//! is trivially low because each `AesCmHmacSha1_80Transform` is used
//! by exactly one forwarder task in one direction (plus the RTCP
//! task at a few packets per second).
//!
//! Replay protection is **on** for both SRTP and SRTCP: every decrypt
//! runs through a sliding-window replay detector
//! ([`SRTP_REPLAY_WINDOW`] / [`SRTCP_REPLAY_WINDOW`] packets), so a
//! captured packet re-injected later is rejected with
//! [`SrtpError::Replayed`].

use std::sync::Mutex;

use smiths_core::{SrtpError, SrtpSuite, SrtpTransform};
use webrtc_srtp::context::Context;
use webrtc_srtp::option::{srtcp_replay_protection, srtp_replay_protection};
use webrtc_srtp::protection_profile::ProtectionProfile;

/// SRTP replay window in packets (RFC 3711 §3.3.2 requires at least
/// 64; 128 tolerates the reordering long paths produce while still
/// fitting comfortably in the detector's bitmap).
pub const SRTP_REPLAY_WINDOW: usize = 128;

/// SRTCP replay window in packets. RTCP runs at a few packets per
/// second, so 64 covers minutes of reordering.
pub const SRTCP_REPLAY_WINDOW: usize = 64;

/// `AES_CM_128_HMAC_SHA1_80` transform. Currently the only suite the
/// engine speaks; adding `AES_CM_128_HMAC_SHA1_32` is a 4-line
/// variant match.
pub struct AesCmHmacSha1_80Transform {
    ctx: Mutex<Context>,
}

impl std::fmt::Debug for AesCmHmacSha1_80Transform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The inner `Context` holds key material; don't leak it via
        // Debug output even though the field lives under a Mutex.
        f.debug_struct("AesCmHmacSha1_80Transform").finish()
    }
}

impl AesCmHmacSha1_80Transform {
    /// Build a transform from SDES master key + salt. `key_material`
    /// is the raw 30 bytes that SDES encodes as
    /// `base64(16-byte-key || 14-byte-salt)` in the SDP
    /// `inline:<...>` parameter.
    ///
    /// Returns [`SrtpError::KeyLength`] if the slice size doesn't
    /// match the suite (16 + 14 = 30). Any internal `webrtc-srtp`
    /// failure surfaces as [`SrtpError::Other`].
    pub fn from_sdes(key_material: &[u8]) -> Result<Self, SrtpError> {
        let suite = SrtpSuite::AesCm128HmacSha1_80;
        if key_material.len() != suite.key_material_len() {
            return Err(SrtpError::KeyLength {
                expected: suite.key_material_len(),
                got: key_material.len(),
            });
        }
        let (key, salt) = key_material.split_at(suite.key_len());
        let ctx = Context::new(
            key,
            salt,
            ProtectionProfile::Aes128CmHmacSha1_80,
            Some(srtp_replay_protection(SRTP_REPLAY_WINDOW)),
            Some(srtcp_replay_protection(SRTCP_REPLAY_WINDOW)),
        )
        .map_err(|e| SrtpError::Other(format!("context init: {e}")))?;
        Ok(Self {
            ctx: Mutex::new(ctx),
        })
    }

    fn with_ctx<T>(&self, f: impl FnOnce(&mut Context) -> T) -> Result<T, SrtpError> {
        let mut guard = self
            .ctx
            .lock()
            .map_err(|_| SrtpError::Other("srtp mutex poisoned".into()))?;
        Ok(f(&mut guard))
    }
}

/// Map a `webrtc-srtp` decrypt failure onto the typed error the bridge
/// branches on: replay-detector rejections and auth-tag mismatches
/// each get their own variant so they can be logged (and, later,
/// counted) separately from malformed input.
fn classify_decrypt_error(e: &webrtc_srtp::Error) -> SrtpError {
    match e {
        webrtc_srtp::Error::SrtpSsrcDuplicated(..)
        | webrtc_srtp::Error::SrtcpSsrcDuplicated(..)
        | webrtc_srtp::Error::ErrDuplicated => SrtpError::Replayed,
        webrtc_srtp::Error::ErrFailedToVerifyAuthTag
        | webrtc_srtp::Error::RtpFailedToVerifyAuthTag
        | webrtc_srtp::Error::RtcpFailedToVerifyAuthTag => SrtpError::AuthFailed,
        other => SrtpError::Other(other.to_string()),
    }
}

impl SrtpTransform for AesCmHmacSha1_80Transform {
    fn protect_rtp(&self, plaintext: &[u8]) -> Result<Vec<u8>, SrtpError> {
        // `Bytes` → `Vec<u8>` reuses the allocation when the buffer is
        // uniquely owned (always the case for a freshly built packet),
        // so this is a move, not a copy.
        self.with_ctx(|ctx| ctx.encrypt_rtp(plaintext))?
            .map(Vec::from)
            .map_err(|e| SrtpError::Other(format!("encrypt: {e}")))
    }

    fn unprotect_rtp(&self, ciphertext: &[u8]) -> Result<Vec<u8>, SrtpError> {
        self.with_ctx(|ctx| ctx.decrypt_rtp(ciphertext))?
            .map(Vec::from)
            .map_err(|e| classify_decrypt_error(&e))
    }

    fn protect_rtcp(&self, plaintext: &[u8]) -> Result<Vec<u8>, SrtpError> {
        self.with_ctx(|ctx| ctx.encrypt_rtcp(plaintext))?
            .map(Vec::from)
            .map_err(|e| SrtpError::Other(format!("encrypt rtcp: {e}")))
    }

    fn unprotect_rtcp(&self, ciphertext: &[u8]) -> Result<Vec<u8>, SrtpError> {
        self.with_ctx(|ctx| ctx.decrypt_rtcp(ciphertext))?
            .map(Vec::from)
            .map_err(|e| classify_decrypt_error(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtcp::build_sr;

    /// Minimal RTP packet for tests: V=2, PT=0 (PCMU), sequence,
    /// timestamp, SSRC, plus a short payload.
    fn rtp_packet(seq: u16, ts: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(12 + payload.len());
        v.push(0x80); // V=2
        v.push(0x00); // M=0, PT=0
        v.extend_from_slice(&seq.to_be_bytes());
        v.extend_from_slice(&ts.to_be_bytes());
        v.extend_from_slice(&ssrc.to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    /// Fixed 30-byte key material (16 + 14). Test-only; any constant
    /// value works because each test builds its own Context pair.
    fn test_km() -> Vec<u8> {
        (0..30u8).collect()
    }

    #[test]
    fn round_trip_through_matched_transforms() {
        // Two independent transforms with the same key material —
        // one encrypts, the other decrypts. This is exactly how the
        // engine wires one side of a bridge: peer-tx key used on
        // local-rx, local-rx key shared between both endpoints.
        let tx = AesCmHmacSha1_80Transform::from_sdes(&test_km()).unwrap();
        let rx = AesCmHmacSha1_80Transform::from_sdes(&test_km()).unwrap();
        let pkt = rtp_packet(1, 0, 0xDEAD_BEEF, b"hi-srtp");
        let ct = tx.protect_rtp(&pkt).expect("encrypt");
        assert_ne!(ct, pkt, "ciphertext should differ from plaintext");
        let pt = rx.unprotect_rtp(&ct).expect("decrypt");
        assert_eq!(pt, pkt, "plaintext round-trips");
    }

    #[test]
    fn wrong_key_fails_authentication() {
        let tx = AesCmHmacSha1_80Transform::from_sdes(&test_km()).unwrap();
        let other_km: Vec<u8> = (30..60u8).collect();
        let rx = AesCmHmacSha1_80Transform::from_sdes(&other_km).unwrap();
        let pkt = rtp_packet(2, 160, 0xCAFE_BABE, b"tampered");
        let ct = tx.protect_rtp(&pkt).unwrap();
        let err = rx.unprotect_rtp(&ct).unwrap_err();
        assert!(
            matches!(err, SrtpError::AuthFailed),
            "expected auth-failure, got {err:?}"
        );
    }

    #[test]
    fn wrong_key_size_rejected_at_construction() {
        let err = AesCmHmacSha1_80Transform::from_sdes(&[0u8; 29]).unwrap_err();
        assert!(
            matches!(
                err,
                SrtpError::KeyLength {
                    expected: 30,
                    got: 29
                }
            ),
            "expected KeyLength{{30, 29}}, got {err:?}"
        );
    }

    #[test]
    fn sequential_packets_retain_round_trip() {
        // SRTP keeps per-SSRC state across packets (rollover counter);
        // the second protect/unprotect pair must still succeed.
        let tx = AesCmHmacSha1_80Transform::from_sdes(&test_km()).unwrap();
        let rx = AesCmHmacSha1_80Transform::from_sdes(&test_km()).unwrap();
        for seq in 1..=4u16 {
            let pkt = rtp_packet(seq, u32::from(seq) * 160, 0x0000_1234, b"seq-test");
            let ct = tx.protect_rtp(&pkt).expect("encrypt");
            let pt = rx.unprotect_rtp(&ct).expect("decrypt");
            assert_eq!(pt, pkt, "packet {seq} must round-trip");
        }
    }

    #[test]
    fn replayed_rtp_packet_is_rejected() {
        let tx = AesCmHmacSha1_80Transform::from_sdes(&test_km()).unwrap();
        let rx = AesCmHmacSha1_80Transform::from_sdes(&test_km()).unwrap();
        let pkt = rtp_packet(10, 1600, 0x0000_5678, b"once-only");
        let ct = tx.protect_rtp(&pkt).unwrap();
        assert_eq!(rx.unprotect_rtp(&ct).unwrap(), pkt, "first copy accepted");
        let err = rx.unprotect_rtp(&ct).unwrap_err();
        assert!(
            matches!(err, SrtpError::Replayed),
            "second copy must be rejected as a replay, got {err:?}"
        );
        // A fresh sequence number is still accepted afterwards.
        let next = rtp_packet(11, 1760, 0x0000_5678, b"next");
        let ct_next = tx.protect_rtp(&next).unwrap();
        assert_eq!(rx.unprotect_rtp(&ct_next).unwrap(), next);
    }

    #[test]
    fn rtp_packet_older_than_the_window_is_rejected() {
        let tx = AesCmHmacSha1_80Transform::from_sdes(&test_km()).unwrap();
        let rx = AesCmHmacSha1_80Transform::from_sdes(&test_km()).unwrap();
        // Encrypt seq 1 but hold it back; deliver a run far past the
        // replay window first, then try to slip seq 1 in.
        let stale = tx
            .protect_rtp(&rtp_packet(1, 0, 0x0000_9999, b"stale"))
            .unwrap();
        for seq in 2..=(u16::try_from(SRTP_REPLAY_WINDOW).expect("window fits u16") + 5) {
            let ct = tx
                .protect_rtp(&rtp_packet(seq, u32::from(seq) * 160, 0x0000_9999, b"run"))
                .unwrap();
            rx.unprotect_rtp(&ct).unwrap();
        }
        let err = rx.unprotect_rtp(&stale).unwrap_err();
        assert!(matches!(err, SrtpError::Replayed), "got {err:?}");
    }

    #[test]
    fn rtcp_round_trips_and_replay_is_rejected() {
        let tx = AesCmHmacSha1_80Transform::from_sdes(&test_km()).unwrap();
        let rx = AesCmHmacSha1_80Transform::from_sdes(&test_km()).unwrap();
        let sr = build_sr(0xAABB_CCDD, 0x1122_3344_5566_7788, 160, 3, 480).to_vec();
        let ct = tx.protect_rtcp(&sr).expect("encrypt rtcp");
        assert!(ct.len() > sr.len(), "SRTCP adds index + auth tag");
        assert_ne!(&ct[8..sr.len()], &sr[8..], "sender info is encrypted");
        let pt = rx.unprotect_rtcp(&ct).expect("decrypt rtcp");
        assert_eq!(pt, sr, "SR round-trips through SRTCP");
        let err = rx.unprotect_rtcp(&ct).unwrap_err();
        assert!(matches!(err, SrtpError::Replayed), "got {err:?}");
    }

    #[test]
    fn rtcp_with_wrong_key_fails_authentication() {
        let tx = AesCmHmacSha1_80Transform::from_sdes(&test_km()).unwrap();
        let other_km: Vec<u8> = (30..60u8).collect();
        let rx = AesCmHmacSha1_80Transform::from_sdes(&other_km).unwrap();
        let sr = build_sr(1, 2, 3, 4, 5).to_vec();
        let ct = tx.protect_rtcp(&sr).unwrap();
        let err = rx.unprotect_rtcp(&ct).unwrap_err();
        assert!(matches!(err, SrtpError::AuthFailed), "got {err:?}");
    }
}
