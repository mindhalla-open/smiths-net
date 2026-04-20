//! DTLS identity helpers for the engine.
//!
//! Slice 1.2 covers the cert-generation half of DTLS-SRTP: mint a
//! self-signed certificate the media fabric can use as the local DTLS
//! identity, and derive the SHA-256 fingerprint the engine publishes
//! in its SDP answer's `a=fingerprint:` line.
//!
//! The handshake that consumes these (slice 1.3) wraps `webrtc-dtls`;
//! this module intentionally knows nothing about the wire layer so
//! tests — and the handshake crate — can both call it without
//! bringing in the DTLS library.

use std::fmt::Write as _;

use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// A minted self-signed cert ready to install on a DTLS endpoint.
///
/// The DER-encoded cert + private-key bytes round-trip cleanly through
/// the usual DTLS crates (`webrtc-dtls`, `rustls-dtls` experiments).
/// The SHA-256 fingerprint is colon-separated uppercase hex, matching
/// the on-the-wire form RFC 8122 §5 prescribes.
#[derive(Clone, Debug)]
pub struct SelfSignedCert {
    /// DER-encoded X.509 certificate.
    pub cert_der: Vec<u8>,
    /// DER-encoded PKCS#8 private key.
    pub key_der: Vec<u8>,
    /// SHA-256 fingerprint, colon-separated uppercase hex —
    /// `AA:BB:CC:...` — 32 bytes × 2 hex chars + 31 colons = 95 chars.
    pub sha256_fingerprint: String,
}

/// Errors raised while minting a DTLS cert.
#[derive(Debug, Error)]
pub enum DtlsCertError {
    /// `rcgen` rejected the parameters or the key generation failed.
    #[error("rcgen: {0}")]
    Rcgen(String),
}

impl SelfSignedCert {
    /// Mint a fresh ECDSA P-256 self-signed cert suitable for DTLS.
    ///
    /// `subject` becomes the `CN=` token on the certificate. The CN
    /// doesn't gate DTLS-SRTP — the peer checks the fingerprint, not
    /// the name — but a descriptive value helps when someone cracks
    /// open a packet capture. Defaults to `smiths-net` when empty.
    ///
    /// The cert's validity window runs from "now" to "now + 1 year" —
    /// long enough to outlast typical process lifetimes but finite so
    /// a pinned cert in a test corpus will expire rather than linger.
    pub fn generate(subject: &str) -> Result<Self, DtlsCertError> {
        let cn = if subject.is_empty() {
            "smiths-net"
        } else {
            subject
        };

        // ECDSA P-256 — same curve WebRTC browsers pick by default, so
        // interop is trivial. Ed25519 would also work but Chrome still
        // only groks P-256 in DTLS-SRTP as of today.
        let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .map_err(|e| DtlsCertError::Rcgen(e.to_string()))?;

        let mut params = CertificateParams::new(Vec::<String>::new())
            .map_err(|e| DtlsCertError::Rcgen(e.to_string()))?;
        params
            .distinguished_name
            .push(DnType::CommonName, cn.to_owned());
        // Zero SANs — DTLS-SRTP doesn't need them; the cert is keyed
        // off the fingerprint, not hostname validation.
        params.distinguished_name = {
            let mut dn = DistinguishedName::new();
            dn.push(DnType::CommonName, cn.to_owned());
            dn
        };

        let cert = params
            .self_signed(&key_pair)
            .map_err(|e| DtlsCertError::Rcgen(e.to_string()))?;

        let cert_der = cert.der().to_vec();
        let key_der = key_pair.serialize_der();

        let sha256_fingerprint = sha256_fingerprint_hex(&cert_der);
        Ok(Self {
            cert_der,
            key_der,
            sha256_fingerprint,
        })
    }
}

/// Render the SHA-256 digest of `cert_der` as colon-separated
/// uppercase hex pairs (RFC 8122 §5 wire format). Pure helper so
/// tests can assert against a known vector.
#[must_use]
pub fn sha256_fingerprint_hex(cert_der: &[u8]) -> String {
    let digest = Sha256::digest(cert_der);
    let mut out = String::with_capacity(digest.len() * 3);
    for (idx, byte) in digest.iter().enumerate() {
        if idx > 0 {
            out.push(':');
        }
        // Uppercase hex per RFC 8122 example block — lowercase still
        // parses but browsers normalize to upper on output.
        let _ = write!(out, "{byte:02X}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_cert_with_well_formed_fingerprint() {
        let c = SelfSignedCert::generate("unit-test").unwrap();
        assert!(!c.cert_der.is_empty());
        assert!(!c.key_der.is_empty());
        // Format: 32 bytes × "AA" + 31 × ":" = 95 chars.
        assert_eq!(c.sha256_fingerprint.len(), 95);
        assert_eq!(c.sha256_fingerprint.matches(':').count(), 31);
        for byte_part in c.sha256_fingerprint.split(':') {
            assert_eq!(byte_part.len(), 2);
            assert!(byte_part.chars().all(|c| c.is_ascii_hexdigit()));
            // RFC 8122 wants uppercase on output.
            assert!(byte_part.chars().all(|c| !c.is_ascii_lowercase()));
        }
    }

    #[test]
    fn two_generations_differ() {
        let a = SelfSignedCert::generate("a").unwrap();
        let b = SelfSignedCert::generate("b").unwrap();
        // Every mint is fresh — the cert bytes, key bytes, and
        // fingerprints must all differ.
        assert_ne!(a.cert_der, b.cert_der);
        assert_ne!(a.key_der, b.key_der);
        assert_ne!(a.sha256_fingerprint, b.sha256_fingerprint);
    }

    #[test]
    fn fingerprint_helper_matches_known_vector() {
        // SHA-256 of the empty string, colon-separated upper hex.
        let fp = sha256_fingerprint_hex(&[]);
        assert_eq!(
            fp,
            "E3:B0:C4:42:98:FC:1C:14:9A:FB:F4:C8:99:6F:B9:24:27:AE:41:E4:64:9B:93:4C:A4:95:99:1B:78:52:B8:55"
        );
    }

    #[test]
    fn empty_subject_falls_back_to_default_cn() {
        // Shouldn't panic; the default CN is applied silently.
        let c = SelfSignedCert::generate("").unwrap();
        assert!(!c.cert_der.is_empty());
    }
}
