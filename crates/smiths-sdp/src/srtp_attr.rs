//! SDES `a=crypto:` attribute parse/generate (RFC 4568 §9.1).
//!
//! Shape on the wire:
//!
//! ```text
//! a=crypto:<tag> <crypto-suite> <key-params> [<session-params>]
//! ```
//!
//! `key-params` for SDES is `inline:<base64(key||salt)>[|<lifetime>][|<mki:mki_len>]`.
//! We parse enough to extract the suite + raw key material and ignore
//! the optional lifetime / MKI tail — they're advisory for the
//! rekeying state machine we haven't wired yet. A future rekey slice
//! will promote them to real fields.
//!
//! Generation is symmetric: caller provides suite + 30 raw bytes,
//! we emit the canonical line.
//!
//! Parsing is intentionally permissive on whitespace; RFC 4568 allows
//! tabs anywhere and trailing comments are silently dropped.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use smiths_core::SrtpSuite;

/// One parsed `a=crypto:` line. Compare on equality is deliberate —
/// the tag + suite + keys fully determine whether two lines match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SdesCrypto {
    /// Numeric tag the offer/answer uses to pair an offer line with
    /// the same-tag answer line.
    pub tag: u32,
    /// Cipher suite (today only [`SrtpSuite::AesCm128HmacSha1_80`]).
    pub suite: SrtpSuite,
    /// Raw `key + salt` bytes (decoded from the `inline:` base64
    /// value). Length equals `suite.key_material_len()`.
    pub key_material: Vec<u8>,
}

/// Problems parsing an `a=crypto:` line.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SdesParseError {
    /// Line didn't start with `a=crypto:` (caller should skip it).
    #[error("not an a=crypto attribute")]
    NotACrypto,
    /// Tag field wasn't a valid u32.
    #[error("invalid tag: {0}")]
    BadTag(String),
    /// Suite name wasn't one we know (see [`SrtpSuite::from_sdp_name`]).
    #[error("unsupported crypto suite: {0}")]
    UnknownSuite(String),
    /// `inline:` parameter missing or `key-params` malformed.
    #[error("missing or malformed key-params: {0}")]
    BadKeyParams(String),
    /// `base64` inside `inline:` didn't decode.
    #[error("inline base64 decode failed: {0}")]
    BadBase64(String),
    /// Decoded key material wasn't the length the suite requires.
    #[error("wrong key-material length: expected {expected}, got {got}")]
    KeyLength {
        /// Expected byte count (`key + salt`).
        expected: usize,
        /// Actual byte count observed on the wire.
        got: usize,
    },
    /// The line looked like `a=crypto:` but had fewer tokens than
    /// `tag suite key-params`.
    #[error("too few tokens in a=crypto")]
    Truncated,
}

impl SdesCrypto {
    /// Parse an `a=crypto:...` line. Accepts the full attribute
    /// including the `a=crypto:` prefix, or the raw value after the
    /// colon — whichever the caller is holding. Returns
    /// [`SdesParseError::NotACrypto`] if the line doesn't start with
    /// `a=crypto:`; it's cheap for the SDP parser to loop and filter.
    pub fn parse(line: &str) -> Result<Self, SdesParseError> {
        let body = line
            .strip_prefix("a=crypto:")
            .ok_or(SdesParseError::NotACrypto)?;
        let mut tokens = body.split_whitespace();
        let tag_str = tokens.next().ok_or(SdesParseError::Truncated)?;
        let suite_str = tokens.next().ok_or(SdesParseError::Truncated)?;
        let key_params = tokens.next().ok_or(SdesParseError::Truncated)?;
        // Trailing session-params (lifetime, MKI) are advisory and
        // ignored here — future rekey work will inspect them.

        let tag: u32 = tag_str
            .parse()
            .map_err(|_| SdesParseError::BadTag(tag_str.to_owned()))?;
        let suite = SrtpSuite::from_sdp_name(suite_str)
            .ok_or_else(|| SdesParseError::UnknownSuite(suite_str.to_owned()))?;

        let inline = key_params
            .strip_prefix("inline:")
            .ok_or_else(|| SdesParseError::BadKeyParams(key_params.to_owned()))?;
        // `inline:` value itself may carry `|lifetime|mki:mki_len`
        // suffixes; SDES separates fields with `|`.
        let b64 = inline
            .split('|')
            .next()
            .ok_or_else(|| SdesParseError::BadKeyParams(inline.to_owned()))?;

        let key_material = BASE64
            .decode(b64.as_bytes())
            .map_err(|e| SdesParseError::BadBase64(e.to_string()))?;
        let expected = suite.key_material_len();
        if key_material.len() != expected {
            return Err(SdesParseError::KeyLength {
                expected,
                got: key_material.len(),
            });
        }
        Ok(Self {
            tag,
            suite,
            key_material,
        })
    }

    /// Render as a canonical SDP attribute line (no trailing CRLF —
    /// the SDP serializer adds it when stitching the document).
    #[must_use]
    pub fn to_sdp_line(&self) -> String {
        format!(
            "a=crypto:{} {} inline:{}",
            self.tag,
            self.suite.sdp_name(),
            BASE64.encode(&self.key_material),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 30-byte key material matching the SDES suite size.
    fn km() -> Vec<u8> {
        (0..30u8).collect()
    }

    #[test]
    fn parse_canonical_line() {
        let km = km();
        let b64 = BASE64.encode(&km);
        let line = format!("a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{b64}");
        let parsed = SdesCrypto::parse(&line).expect("parse");
        assert_eq!(parsed.tag, 1);
        assert_eq!(parsed.suite, SrtpSuite::AesCm128HmacSha1_80);
        assert_eq!(parsed.key_material, km);
    }

    #[test]
    fn parse_with_lifetime_and_mki_suffix_ignored() {
        let km = km();
        let b64 = BASE64.encode(&km);
        // Real-world UAs often append `|2^31|1:4`.
        let line = format!("a=crypto:42 AES_CM_128_HMAC_SHA1_80 inline:{b64}|2^31|1:4");
        let parsed = SdesCrypto::parse(&line).expect("parse");
        assert_eq!(parsed.tag, 42);
        assert_eq!(parsed.key_material, km);
    }

    #[test]
    fn round_trip_to_sdp_line_and_back() {
        let km = km();
        let orig = SdesCrypto {
            tag: 7,
            suite: SrtpSuite::AesCm128HmacSha1_80,
            key_material: km,
        };
        let line = orig.to_sdp_line();
        let parsed = SdesCrypto::parse(&line).unwrap();
        assert_eq!(parsed, orig);
    }

    #[test]
    fn not_a_crypto_line_is_rejected_cheaply() {
        assert_eq!(
            SdesCrypto::parse("a=rtpmap:0 PCMU/8000"),
            Err(SdesParseError::NotACrypto)
        );
        assert_eq!(
            SdesCrypto::parse("random garbage"),
            Err(SdesParseError::NotACrypto)
        );
    }

    #[test]
    fn unknown_suite_rejected() {
        let b64 = BASE64.encode(km());
        let line = format!("a=crypto:1 AES_256_GCM inline:{b64}");
        assert!(matches!(
            SdesCrypto::parse(&line),
            Err(SdesParseError::UnknownSuite(_))
        ));
    }

    #[test]
    fn short_key_material_rejected() {
        // 29 bytes instead of 30.
        let short = BASE64.encode([0u8; 29]);
        let line = format!("a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{short}");
        let err = SdesCrypto::parse(&line).unwrap_err();
        assert!(
            matches!(
                err,
                SdesParseError::KeyLength {
                    expected: 30,
                    got: 29
                }
            ),
            "expected KeyLength error, got {err:?}"
        );
    }

    #[test]
    fn truncated_line_rejected() {
        assert!(matches!(
            SdesCrypto::parse("a=crypto:1"),
            Err(SdesParseError::Truncated)
        ));
    }
}
