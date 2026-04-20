//! [`SrtpTransform`] implementation backed by `webrtc-srtp`.
//!
//! Per-direction primitive: one instance encrypts engine→peer, a
//! separate one decrypts peer→engine. The bridge composes four of
//! these per call (two legs × two directions) and drives them from
//! the forwarder hot path.
//!
//! `webrtc-srtp`'s [`Context`] is `&mut`-stateful (per-SSRC rollover
//! counters, replay detector), so we wrap it in a [`tokio::sync::Mutex`]
//! to match the `SrtpTransform` trait's `&self` signature. Contention
//! is trivially low because each `AesCmHmacSha1_80Transform` is used
//! by exactly one forwarder task in one direction.

use std::sync::Mutex;

use smiths_core::{SrtpError, SrtpSuite, SrtpTransform};
use webrtc_srtp::context::Context;
use webrtc_srtp::protection_profile::ProtectionProfile;

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
            None,
            None,
        )
        .map_err(|e| SrtpError::Other(format!("context init: {e}")))?;
        Ok(Self {
            ctx: Mutex::new(ctx),
        })
    }
}

impl SrtpTransform for AesCmHmacSha1_80Transform {
    fn protect_rtp(&self, plaintext: &[u8]) -> Result<Vec<u8>, SrtpError> {
        let mut guard = self
            .ctx
            .lock()
            .map_err(|_| SrtpError::Other("srtp mutex poisoned".into()))?;
        let bytes = guard
            .encrypt_rtp(plaintext)
            .map_err(|e| SrtpError::Other(format!("encrypt: {e}")))?;
        Ok(bytes.to_vec())
    }

    fn unprotect_rtp(&self, ciphertext: &[u8]) -> Result<Vec<u8>, SrtpError> {
        let mut guard = self
            .ctx
            .lock()
            .map_err(|_| SrtpError::Other("srtp mutex poisoned".into()))?;
        match guard.decrypt_rtp(ciphertext) {
            Ok(bytes) => Ok(bytes.to_vec()),
            Err(e) => {
                let msg = e.to_string();
                // `webrtc-srtp` uses distinct error strings for auth
                // failure vs other decrypt errors; map the known
                // auth-failure strings to our typed variant so the
                // bridge can drop cleanly and log separately.
                if msg.contains("authentication") || msg.contains("Auth") {
                    Err(SrtpError::AuthFailed)
                } else {
                    Err(SrtpError::Other(msg))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            matches!(err, SrtpError::AuthFailed | SrtpError::Other(_)),
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
}
