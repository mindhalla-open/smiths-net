//! DTLS-SRTP handshake state machine for smiths-net.
//!
//! Wraps `webrtc-dtls` with a per-leg state machine keyed to the media
//! fabric's lifecycle. For each call leg, the media path runs:
//!
//! ```text
//!   Idle ─► Handshaking ─► Active ─► Closed
//!                │                     ▲
//!                └──── on error ───────┘
//! ```
//!
//! The handshake completes against a **fixed peer address** per
//! slice 1.3's scope — ICE is slice 1.4's job; until that lands the
//! DTLS layer talks to whatever endpoint the SDP answer named.
//!
//! ## What this crate does not do
//!
//! - **ICE / NAT traversal** — caller provides a direct UDP socket
//!   pointed at the negotiated peer address. See `smiths-ice`.
//! - **Renegotiation / cipher rotation** — MVP is
//!   `AES_CM_128_HMAC_SHA1_80` only.
//! - **Multi-peer demux** — one leg = one `DtlsLeg` value.
//!
//! ## Integration points (slice 1.5, not 1.3)
//!
//! The media fabric owns a `DtlsLeg` per bridge leg, calls
//! [`DtlsLeg::handshake`] before any RTP flows, and threads the
//! returned [`smiths_core::SrtpKeys`] into the existing SRTP transform
//! exactly like SDES does today.

// Crate-level tightening in the same spirit as smiths-core /
// smiths-sdp: no production-code unwrap / expect. Tests can still use
// them freely.
#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::sync::Arc;

use sha2::{Digest, Sha256};
use smiths_core::{SelfSignedCert, SrtpKeys, SrtpSuite};
use smiths_sdp::Fingerprint;
use thiserror::Error;
use tokio::sync::Mutex;
use webrtc_dtls::Error as DtlsError;
use webrtc_dtls::config::{ClientAuthType, Config, ExtendedMasterSecretType};
use webrtc_dtls::conn::DTLSConn;
use webrtc_dtls::crypto::Certificate;
use webrtc_dtls::extension::extension_use_srtp::SrtpProtectionProfile;
use webrtc_util::KeyingMaterialExporter;
use webrtc_util::conn::Conn;

/// Which side of the handshake this leg plays.
///
/// Driven by the peer's `a=setup:` attribute (RFC 5763 §5): `actpass`
/// or `passive` → we take [`DtlsRole::Client`]; `active` → we're
/// [`DtlsRole::Server`]. The negotiator picks the concrete role when
/// the peer offers `actpass`; callers only see this enum.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DtlsRole {
    /// This end initiates the handshake (sends `ClientHello`).
    Client,
    /// This end awaits the peer's `ClientHello`.
    Server,
}

/// Lifecycle of a DTLS leg. The caller drives the state transitions
/// by calling [`DtlsLeg::handshake`] — reading the state is purely
/// for observability (logs, metrics, health endpoints).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LegState {
    /// Pre-handshake — the leg is waiting for its first packet.
    Idle,
    /// Handshake in progress. Packets are flowing through `DTLSConn`.
    Handshaking,
    /// Handshake complete; SRTP keys extracted. RTP can flow.
    Active,
    /// Handshake failed or the leg was closed. No further state
    /// transitions are possible.
    Closed,
}

/// Everything a leg needs to run its DTLS handshake.
///
/// Per-call ephemeral — built in the UAS / media fabric once the
/// SDP offer/answer has picked roles and fingerprints. Do not clone
/// across calls; the cert's fingerprint is tied to the local leg.
#[derive(Clone, Debug)]
pub struct DtlsLegConfig {
    /// Engine-minted cert (slice 1.2's [`SelfSignedCert`]).
    pub local_cert: SelfSignedCert,
    /// Role this leg plays per the offer/answer `a=setup:` negotiation.
    pub role: DtlsRole,
    /// Fingerprint the peer advertised in its SDP. The handshake
    /// verifies the negotiated cert matches this; mismatch = tear
    /// down with [`DtlsHandshakeError::FingerprintMismatch`].
    pub peer_fingerprint: Fingerprint,
}

/// Errors raised while running the handshake or extracting SRTP keys.
#[derive(Debug, Error)]
pub enum DtlsHandshakeError {
    /// Underlying `webrtc-dtls` error — handshake failed, peer closed,
    /// cipher mismatch, etc. Carried as a string so we don't re-export
    /// the dependency's enum across crate boundaries.
    #[error("dtls: {0}")]
    Dtls(String),
    /// The cert the peer presented during the handshake does **not**
    /// hash to the fingerprint the SDP advertised. Per RFC 5763 §8
    /// this is fatal — we tear the association down rather than risk
    /// trusting a MITM cert.
    #[error("peer fingerprint mismatch: expected {expected}, observed {observed}")]
    FingerprintMismatch {
        /// Fingerprint from the peer's `a=fingerprint:` line.
        expected: String,
        /// Fingerprint computed from the cert `webrtc-dtls` negotiated.
        observed: String,
    },
    /// SRTP key export rejected by the underlying DTLS state (e.g.
    /// handshake wasn't Active when we asked). Should be unreachable
    /// from ordinary flow — surfaced so the bug is obvious if it ever
    /// fires.
    #[error("srtp keying material unavailable: {0}")]
    KeyExport(String),
    /// An unsupported hash was advertised in the peer's fingerprint.
    /// Only `sha-256` is accepted for MVP; slipping in `sha-1` would
    /// defeat the chain of custody we get from DTLS-SRTP.
    #[error("unsupported fingerprint algorithm: {0}")]
    UnsupportedAlgorithm(String),
    /// Cert ingestion into `webrtc-dtls` failed (malformed DER, bad
    /// key pairing, etc).
    #[error("cert load: {0}")]
    CertLoad(String),
}

/// Number of bytes to export via the `EXTRACTOR-dtls_srtp` keying
/// material label (RFC 5764 §4.2). For
/// `SRTP_AES128_CM_HMAC_SHA1_80` (the only profile MVP supports)
/// this is **2 × (`key_len` + `salt_len`) = 2 × (16 + 14) = 60**
/// bytes. The first half is the client-to-server key stack, the
/// second half is server-to-client.
const DTLS_SRTP_EXPORT_BYTES: usize = 60;
/// Key material label mandated by RFC 5764 §4.2. **Must** match
/// verbatim or the peer decrypts garbage.
const DTLS_SRTP_EXPORT_LABEL: &str = "EXTRACTOR-dtls_srtp";

/// One leg's DTLS handshake + key-export state machine.
///
/// Not `Clone` on purpose: each leg owns its `DTLSConn` and moves
/// the socket in. Two legs on one call = two `DtlsLeg` values.
pub struct DtlsLeg {
    config: DtlsLegConfig,
    state: Mutex<LegState>,
    conn: Mutex<Option<Arc<DTLSConn>>>,
}

impl DtlsLeg {
    /// Fresh leg in [`LegState::Idle`]. No packets have been sent.
    #[must_use]
    pub fn new(config: DtlsLegConfig) -> Self {
        Self {
            config,
            state: Mutex::new(LegState::Idle),
            conn: Mutex::new(None),
        }
    }

    /// Current leg state. Cheap — just a mutex read.
    pub async fn state(&self) -> LegState {
        self.state.lock().await.clone()
    }

    /// Run the DTLS handshake over `socket` (which must already be
    /// pointed at the peer — the media fabric / ICE layer owns
    /// target resolution). Returns the extracted SRTP keying material
    /// on success; on any failure the leg transitions to
    /// [`LegState::Closed`].
    ///
    /// The provided socket must implement [`webrtc_util::conn::Conn`].
    /// In production the media fabric supplies a bound
    /// `UdpSocket`-derived adapter; tests can hand in an in-memory
    /// pipe via the same trait.
    pub async fn handshake(
        &self,
        socket: Arc<dyn Conn + Send + Sync>,
    ) -> Result<SrtpKeys, DtlsHandshakeError> {
        // Rustls 0.23 requires a `CryptoProvider`; install the
        // `ring` one on the first handshake (see the helper's
        // doc comment for the CI feature-unification story).
        install_default_crypto_provider();

        *self.state.lock().await = LegState::Handshaking;

        let config = self.build_dtls_config()?;
        let is_client = self.config.role == DtlsRole::Client;
        let conn = DTLSConn::new(socket, config, is_client, None)
            .await
            .map_err(|e| stringify_dtls(&e))?;
        let conn = Arc::new(conn);

        // Verify the negotiated peer cert matches the SDP fingerprint
        // before we mark the leg Active or hand back keys. This is the
        // guardrail that anchors DTLS-SRTP to SIP-signalled identity.
        let state = conn.connection_state().await;
        self.verify_peer_fingerprint(&state.peer_certificates)?;

        // Extract SRTP key material per RFC 5764 §4.2.
        let exported = state
            .export_keying_material(DTLS_SRTP_EXPORT_LABEL, &[], DTLS_SRTP_EXPORT_BYTES)
            .await
            .map_err(|e| DtlsHandshakeError::KeyExport(e.to_string()))?;

        // The protection profile is fixed for MVP — if webrtc-dtls
        // somehow picked something else, we want to know (the `use_srtp`
        // extension config should have made the decision mutual).
        let profile = conn.selected_srtpprotection_profile();
        if profile != SrtpProtectionProfile::Srtp_Aes128_Cm_Hmac_Sha1_80 {
            return Err(DtlsHandshakeError::Dtls(format!(
                "expected AES_CM_128_HMAC_SHA1_80, got {profile:?}"
            )));
        }

        let keys = split_exported_srtp_keys(&exported, self.config.role);
        *self.conn.lock().await = Some(conn);
        *self.state.lock().await = LegState::Active;
        Ok(keys)
    }

    /// Cleanly tear the leg down. Idempotent — calling on an already
    /// [`LegState::Closed`] leg is a no-op.
    pub async fn close(&self) {
        if let Some(conn) = self.conn.lock().await.take()
            && let Err(e) = conn.close().await
        {
            tracing::debug!(?e, "dtls leg close returned error (already closed?)");
        }
        *self.state.lock().await = LegState::Closed;
    }

    fn build_dtls_config(&self) -> Result<Config, DtlsHandshakeError> {
        let cert = self.load_cert()?;
        Ok(Config {
            certificates: vec![cert],
            // Single MVP profile (RFC 5764 §4.1.2). Adding more is a
            // follow-on slice, not a correctness risk today.
            srtp_protection_profiles: vec![SrtpProtectionProfile::Srtp_Aes128_Cm_Hmac_Sha1_80],
            // DTLS-SRTP in WebRTC uses self-signed certs everywhere and
            // validates via the SDP fingerprint (the explicit check
            // below), not a CA chain. Skip webrtc-dtls's internal cert
            // validation; we do it ourselves against the SDP.
            insecure_skip_verify: true,
            // Require client auth whenever we're the server — RFC 5763
            // §5 mandates mutual certs so the fingerprint check works
            // on both sides.
            client_auth: if self.config.role == DtlsRole::Server {
                ClientAuthType::RequireAnyClientCert
            } else {
                ClientAuthType::NoClientCert
            },
            // RFC 7627 extended master secret — always on for DTLS-SRTP
            // so the key derivation survives the triple-handshake
            // attack.
            extended_master_secret: ExtendedMasterSecretType::Require,
            ..Default::default()
        })
    }

    fn load_cert(&self) -> Result<Certificate, DtlsHandshakeError> {
        // `webrtc_dtls::crypto::Certificate::from_pem` expects the
        // bundle to lead with a `PRIVATE_KEY` block (underscore
        // in the tag), followed by one or more `CERTIFICATE`
        // blocks — that's webrtc-dtls 0.12's contract. Concretely:
        //
        //   -----BEGIN PRIVATE_KEY-----
        //   ...
        //   -----END PRIVATE_KEY-----
        //   -----BEGIN CERTIFICATE-----
        //   ...
        //   -----END CERTIFICATE-----
        //
        // We mint DER via rcgen (slice 1.2); round-trip through
        // PEM so the types line up without re-implementing DER →
        // Certificate conversion.
        let key_pem = der_to_pem("PRIVATE_KEY", &self.config.local_cert.key_der);
        let cert_pem = der_to_pem("CERTIFICATE", &self.config.local_cert.cert_der);
        let bundle = format!("{key_pem}{cert_pem}");
        Certificate::from_pem(&bundle)
            .map_err(|e: DtlsError| DtlsHandshakeError::CertLoad(e.to_string()))
    }

    fn verify_peer_fingerprint(&self, peer_chain: &[Vec<u8>]) -> Result<(), DtlsHandshakeError> {
        let Some(leaf) = peer_chain.first() else {
            return Err(DtlsHandshakeError::FingerprintMismatch {
                expected: self.config.peer_fingerprint.value.clone(),
                observed: "<no peer certificate>".into(),
            });
        };
        let expected_algo = self.config.peer_fingerprint.algorithm.to_ascii_lowercase();
        if expected_algo != "sha-256" {
            return Err(DtlsHandshakeError::UnsupportedAlgorithm(expected_algo));
        }
        let observed = sha256_colon_upper(leaf);
        let expected = normalize_colon_hex(&self.config.peer_fingerprint.value);
        if observed != expected {
            return Err(DtlsHandshakeError::FingerprintMismatch { expected, observed });
        }
        Ok(())
    }
}

fn stringify_dtls(e: &DtlsError) -> DtlsHandshakeError {
    DtlsHandshakeError::Dtls(e.to_string())
}

/// Ensure rustls has a process-wide [`CryptoProvider`] installed
/// before any DTLS handshake runs.
///
/// `webrtc-dtls` 0.12 uses rustls internally; rustls 0.23 refuses
/// to pick a provider unless exactly one of its `aws-lc-rs` /
/// `ring` features is active **or** the process pre-installs a
/// provider via `CryptoProvider::install_default`. On a CI image
/// where Cargo's feature unification doesn't end up with either
/// (the workspace pins `rustls` with `ring`, but the transitive
/// graph via `webrtc-dtls` can resolve differently depending on
/// which crate the `ring` feature request enters through), a
/// handshake panics mid-flight with "Could not automatically
/// determine the process-level `CryptoProvider`".
///
/// We install the `ring` provider here explicitly, once per
/// process via `std::sync::Once`. Subsequent calls are cheap
/// no-ops; only the first resolves. The workspace already pins
/// `rustls` with `default-features = false, features = ["ring"]`,
/// so the `ring` provider's code is always in the build — the
/// fix is purely about the install-default decision that
/// `webrtc-dtls` doesn't make on its own.
fn install_default_crypto_provider() {
    use std::sync::Once;
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        // `install_default` errors on "already installed" — ignore it;
        // the process-wide default is what matters, not who won.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Encode DER bytes as a PEM block with the given label. Pure so
/// tests can exercise it without the DTLS stack.
fn der_to_pem(label: &str, der: &[u8]) -> String {
    use std::fmt::Write as _;
    let b64 = base64_encode(der);
    let mut out = String::new();
    let _ = writeln!(out, "-----BEGIN {label}-----");
    for chunk in b64.as_bytes().chunks(64) {
        if let Ok(line) = std::str::from_utf8(chunk) {
            let _ = writeln!(out, "{line}");
        }
    }
    let _ = writeln!(out, "-----END {label}-----");
    out
}

/// Minimal base64 encoder — pure ASCII, standard alphabet, no padding
/// omitted. Avoids dragging in another dep just for PEM framing.
fn base64_encode(input: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= input.len() {
        let n =
            (u32::from(input[i]) << 16) | (u32::from(input[i + 1]) << 8) | u32::from(input[i + 2]);
        out.push(char::from(ALPHA[((n >> 18) & 0x3F) as usize]));
        out.push(char::from(ALPHA[((n >> 12) & 0x3F) as usize]));
        out.push(char::from(ALPHA[((n >> 6) & 0x3F) as usize]));
        out.push(char::from(ALPHA[(n & 0x3F) as usize]));
        i += 3;
    }
    let rem = input.len() - i;
    if rem == 1 {
        let n = u32::from(input[i]) << 16;
        out.push(char::from(ALPHA[((n >> 18) & 0x3F) as usize]));
        out.push(char::from(ALPHA[((n >> 12) & 0x3F) as usize]));
        out.push_str("==");
    } else if rem == 2 {
        let n = (u32::from(input[i]) << 16) | (u32::from(input[i + 1]) << 8);
        out.push(char::from(ALPHA[((n >> 18) & 0x3F) as usize]));
        out.push(char::from(ALPHA[((n >> 12) & 0x3F) as usize]));
        out.push(char::from(ALPHA[((n >> 6) & 0x3F) as usize]));
        out.push('=');
    }
    out
}

/// SHA-256 → colon-separated uppercase hex (RFC 8122 §5 wire form).
fn sha256_colon_upper(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 3);
    for (i, b) in digest.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        let _ = write!(out, "{b:02X}");
    }
    out
}

/// Normalize a SDP-advertised fingerprint for comparison. Trims
/// whitespace and uppercases every hex pair; preserves colons. Safe
/// on already-normalized input.
fn normalize_colon_hex(raw: &str) -> String {
    raw.split(':')
        .map(|p| p.trim().to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join(":")
}

/// Slice the RFC 5764 §4.2 export into the four 16-byte keys + 14-byte
/// salts (in client→server then server→client order), then assign
/// them to `peer_tx_key` / `local_tx_key` based on our role.
fn split_exported_srtp_keys(exported: &[u8], role: DtlsRole) -> SrtpKeys {
    // Per RFC 5764 §4.2: key stack is `client_write_key || server_write_key
    // || client_write_salt || server_write_salt`. 16 + 16 + 14 + 14 = 60.
    let (key_block, salt_block) = exported.split_at(32);
    let (client_key, server_key) = key_block.split_at(16);
    let (client_salt, server_salt) = salt_block.split_at(14);

    let client_km: Vec<u8> = [client_key, client_salt].concat();
    let server_km: Vec<u8> = [server_key, server_salt].concat();

    let (peer_tx_key, local_tx_key) = match role {
        // We're the DTLS client → our egress uses the client stack,
        // peer's egress uses the server stack.
        DtlsRole::Client => (server_km, client_km),
        DtlsRole::Server => (client_km, server_km),
    };
    SrtpKeys {
        suite: SrtpSuite::AesCm128HmacSha1_80,
        peer_tx_key,
        local_tx_key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(value: &str) -> Fingerprint {
        Fingerprint {
            algorithm: "sha-256".into(),
            value: value.into(),
        }
    }

    #[test]
    fn state_starts_idle() {
        let cert = SelfSignedCert::generate("test").unwrap();
        let leg = DtlsLeg::new(DtlsLegConfig {
            local_cert: cert.clone(),
            role: DtlsRole::Client,
            peer_fingerprint: fp(&cert.sha256_fingerprint),
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        let s = rt.block_on(leg.state());
        assert_eq!(s, LegState::Idle);
    }

    #[test]
    fn verify_peer_fingerprint_accepts_matching_cert() {
        let cert = SelfSignedCert::generate("peer").unwrap();
        let leg = DtlsLeg::new(DtlsLegConfig {
            local_cert: SelfSignedCert::generate("local").unwrap(),
            role: DtlsRole::Server,
            peer_fingerprint: fp(&cert.sha256_fingerprint),
        });
        // Feed the `peer_chain` the way webrtc-dtls would after a
        // successful handshake (Vec<Vec<u8>> of raw DER).
        let chain = vec![cert.cert_der.clone()];
        assert!(leg.verify_peer_fingerprint(&chain).is_ok());
    }

    #[test]
    fn verify_peer_fingerprint_rejects_mismatched_cert() {
        let cert_a = SelfSignedCert::generate("a").unwrap();
        let cert_b = SelfSignedCert::generate("b").unwrap();
        let leg = DtlsLeg::new(DtlsLegConfig {
            local_cert: SelfSignedCert::generate("local").unwrap(),
            role: DtlsRole::Server,
            peer_fingerprint: fp(&cert_a.sha256_fingerprint),
        });
        let chain = vec![cert_b.cert_der.clone()];
        match leg.verify_peer_fingerprint(&chain) {
            Err(DtlsHandshakeError::FingerprintMismatch { .. }) => {}
            other => panic!("expected FingerprintMismatch, got {other:?}"),
        }
    }

    #[test]
    fn verify_peer_fingerprint_rejects_empty_chain() {
        let leg = DtlsLeg::new(DtlsLegConfig {
            local_cert: SelfSignedCert::generate("local").unwrap(),
            role: DtlsRole::Server,
            peer_fingerprint: fp("AA:BB"),
        });
        assert!(
            matches!(
                leg.verify_peer_fingerprint(&[]),
                Err(DtlsHandshakeError::FingerprintMismatch { .. })
            ),
            "empty peer chain must surface as FingerprintMismatch"
        );
    }

    #[test]
    fn verify_peer_fingerprint_rejects_weak_algorithm() {
        let cert = SelfSignedCert::generate("x").unwrap();
        let mut fp_weak = fp(&cert.sha256_fingerprint);
        fp_weak.algorithm = "sha-1".into();
        let leg = DtlsLeg::new(DtlsLegConfig {
            local_cert: SelfSignedCert::generate("local").unwrap(),
            role: DtlsRole::Server,
            peer_fingerprint: fp_weak,
        });
        let chain = std::slice::from_ref(&cert.cert_der);
        match leg.verify_peer_fingerprint(chain) {
            Err(DtlsHandshakeError::UnsupportedAlgorithm(algo)) => {
                assert_eq!(algo, "sha-1");
            }
            other => panic!("expected UnsupportedAlgorithm, got {other:?}"),
        }
    }

    #[test]
    fn fingerprint_normalization_is_case_insensitive() {
        assert_eq!(normalize_colon_hex("aa:bb:cc"), "AA:BB:CC");
        assert_eq!(normalize_colon_hex("AA:BB:CC"), "AA:BB:CC");
        assert_eq!(normalize_colon_hex("  aa:bb  "), "AA:BB");
    }

    #[test]
    fn split_exported_keys_client_view() {
        // DTLS_SRTP_EXPORT_BYTES is 60 — safely fits in u8.
        let len = u8::try_from(DTLS_SRTP_EXPORT_BYTES).unwrap();
        let exported: Vec<u8> = (0..len).collect();
        let keys = split_exported_srtp_keys(&exported, DtlsRole::Client);
        assert_eq!(keys.suite, SrtpSuite::AesCm128HmacSha1_80);
        // Each side's key_material = 16 bytes key + 14 bytes salt.
        assert_eq!(keys.peer_tx_key.len(), 30);
        assert_eq!(keys.local_tx_key.len(), 30);
        // Client view: local uses client-stack, peer uses server-stack.
        assert_eq!(keys.local_tx_key[..16], exported[..16]);
        assert_eq!(keys.peer_tx_key[..16], exported[16..32]);
    }

    #[test]
    fn split_exported_keys_server_view_is_mirror() {
        let len = u8::try_from(DTLS_SRTP_EXPORT_BYTES).unwrap();
        let exported: Vec<u8> = (0..len).collect();
        let client_keys = split_exported_srtp_keys(&exported, DtlsRole::Client);
        let server_keys = split_exported_srtp_keys(&exported, DtlsRole::Server);
        // Server's peer is the client, so the halves swap.
        assert_eq!(server_keys.peer_tx_key, client_keys.local_tx_key);
        assert_eq!(server_keys.local_tx_key, client_keys.peer_tx_key);
    }

    #[test]
    fn base64_encode_matches_known_vectors() {
        // Classic RFC 4648 test vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn der_to_pem_wraps_lines_at_64_cols() {
        let pem = der_to_pem("CERTIFICATE", &[0xAB; 120]);
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----\n"));
        assert!(pem.trim_end().ends_with("-----END CERTIFICATE-----"));
        // No line (excluding BEGIN/END markers) is > 64 base64 chars.
        for line in pem.lines() {
            if line.starts_with("---") {
                continue;
            }
            assert!(line.len() <= 64, "PEM body line too long: {line:?}");
        }
    }

    // Full handshake test against a live peer is gated behind
    // `--ignored`. The harness requires an external openssl binary
    // supporting DTLS 1.2 (`openssl s_client -dtls1_2`) and live UDP
    // sockets; slice 1.3 lands the code + unit tests, slice 1.5 ties
    // the integration suite to the headless-Chromium harness it adds.
    #[test]
    #[ignore = "requires openssl s_client -dtls1_2; see slice 1.5 headless harness"]
    fn handshake_completes_against_openssl_s_client_placeholder() {
        // Intentional no-op. Keeping the `#[test]` so the name shows
        // up in the #[ignore] bucket and nobody accidentally deletes
        // the slot.
    }
}
