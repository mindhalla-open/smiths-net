//! Wire schema shared by the engine core, WASM guests, and sidecar
//! plugins.
//!
//! ## Design
//!
//! The schema is expressed as `prost`-deriving Rust types — no
//! `build.rs`, no `protoc` toolchain needed at compile time. Each
//! message carries explicit field numbers so rename-safe forward
//! compatibility is locked in. When the sidecar path eventually
//! migrates from JSON-RPC to protobuf stdio, the same types
//! serialize both sides.
//!
//! ## v1 envelope
//!
//! `Envelope` is the top-level frame on the plugin transport. It
//! carries exactly one of:
//!
//! - `Request` — engine → plugin invocation with a method name and
//!   opaque JSON-encoded params.
//! - `Response` — plugin → engine reply (result or error).
//! - `Notification` — plugin → engine fire-and-forget signal (no `id`).
//!
//! `params` / `result` / `error_message` are free-form UTF-8 — we
//! don't tie the wire schema to a fixed control-plane shape; the
//! MCP layer owns that. What's frozen here is the **framing**.
//!
//! ## Stability
//!
//! v1 is frozen. New fields may be added with fresh tag numbers.
//! Existing tags never change meaning.

// `no_std` survives only when the flatbuffers feature is off;
// the `flatbuffers` crate pulls `std`. Plugin authors who need a
// `no_std` build stay on the default-features-off compile
// (`smiths-proto = { version = ..., default-features = false }`).
#![cfg_attr(not(feature = "flatbuffers"), no_std)]
// Slice 1.7: match smiths-core / smiths-sdp. Every public wire
// type carries a doc line so downstream plugin authors can read
// the generated types without the .proto source.
#![warn(missing_docs)]
#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use prost::Message;

#[cfg(feature = "flatbuffers")]
pub mod flatbuffers_io;

/// Envelope carrying exactly one of request / response / notification.
/// A single optional `kind` field on the wire. Extra `oneof` members
/// can be added later without breaking older peers.
#[derive(Clone, PartialEq, Message)]
pub struct Envelope {
    /// The payload. Exactly one variant is set; wire peers that see
    /// nothing set should treat the frame as malformed and close.
    #[prost(oneof = "envelope::Kind", tags = "1, 2, 3")]
    pub kind: Option<envelope::Kind>,
}

/// Namespace holding the `oneof` variants for [`Envelope`].
pub mod envelope {
    use super::{Notification, Request, Response};
    use prost::Oneof;

    /// Exactly one of these is set on every envelope.
    #[derive(Clone, PartialEq, Oneof)]
    pub enum Kind {
        /// Engine → plugin invocation.
        #[prost(message, tag = "1")]
        Request(Request),
        /// Plugin → engine reply.
        #[prost(message, tag = "2")]
        Response(Response),
        /// Plugin → engine one-way signal.
        #[prost(message, tag = "3")]
        Notification(Notification),
    }
}

/// Engine-to-plugin invocation.
#[derive(Clone, PartialEq, Message)]
pub struct Request {
    /// Caller-chosen correlation id; echoed in the matching
    /// [`Response`]. Monotonic `u64` on the engine side; plugins
    /// shouldn't inspect it.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// Method name (`"synthesize"`, `"transcribe"`, ...).
    #[prost(string, tag = "2")]
    pub method: String,
    /// Opaque UTF-8 params — usually JSON, but any encoding the
    /// control plane agreed on works. Empty string = no params.
    #[prost(string, tag = "3")]
    pub params: String,
}

/// Plugin-to-engine reply. Exactly one of `result` (non-empty) or
/// `error_message` (non-empty) is expected. Both empty = unset reply
/// (malformed). Both set = undefined behavior; peers may pick either.
#[derive(Clone, PartialEq, Message)]
pub struct Response {
    /// Correlation id echoed from the triggering [`Request`].
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// UTF-8 result payload (typically JSON). Empty means the reply
    /// is an error; see `error_message`.
    #[prost(string, tag = "2")]
    pub result: String,
    /// Human-readable error string if the call failed.
    #[prost(string, tag = "3")]
    pub error_message: String,
    /// Optional machine-readable error code paralleling JSON-RPC
    /// (`-32601` = method not found, etc.). `0` when no error.
    #[prost(int32, tag = "4")]
    pub error_code: i32,
}

/// Plugin-to-engine fire-and-forget signal. No id, no reply path.
#[derive(Clone, PartialEq, Message)]
pub struct Notification {
    /// Notification method (`"emit_partial"`, `"log"`, ...).
    #[prost(string, tag = "1")]
    pub method: String,
    /// Opaque UTF-8 params (JSON by convention).
    #[prost(string, tag = "2")]
    pub params: String,
}

/// Convenience: encode a message to a `Vec<u8>`.
///
/// Call-site sugar — prost's `Message::encode_to_vec` already exists
/// but needs `use prost::Message;` in the caller's scope. Re-exposing
/// it here means downstream code doesn't have to import the trait.
pub fn encode<M: Message>(m: &M) -> Vec<u8> {
    m.encode_to_vec()
}

/// Convenience: decode a message from a byte slice. Returns
/// [`prost::DecodeError`] on malformed input.
pub fn decode<M: Message + Default>(bytes: &[u8]) -> Result<M, prost::DecodeError> {
    M::decode(bytes)
}

// ---------------------------------------------------------------------
// Slice 5.2 / P18 — pluggable wire format
// ---------------------------------------------------------------------

/// Per-packet RTP frame the engine hands to a streaming-RTP plugin
/// (the target of the `media.streaming_rtp` capability, slice 2.5).
/// Roughly 50 frames/sec/leg at 20 ms packetization — the hot path
/// the wire-format trait exists to optimise.
#[derive(Clone, PartialEq, Message)]
pub struct RtpFrame {
    /// Call-ID the frame belongs to. Kept so a plugin receiving
    /// frames from multiple concurrent calls can disambiguate
    /// without a side-channel.
    #[prost(string, tag = "1")]
    pub call_id: String,
    /// RTP synchronization source (SSRC), post-rewrite.
    #[prost(uint32, tag = "2")]
    pub ssrc: u32,
    /// RTP sequence number. Wraps at `u16::MAX`.
    #[prost(uint32, tag = "3")]
    pub sequence: u32,
    /// RTP timestamp (in codec's clock units).
    #[prost(uint32, tag = "4")]
    pub timestamp: u32,
    /// RTP payload type (`0` = PCMU, `96`+ = dynamic, …).
    #[prost(uint32, tag = "5")]
    pub payload_type: u32,
    /// Direction label (`"a_to_b"` / `"b_to_a"`). Short string; kept
    /// human-readable to keep eyeball debugging easy.
    #[prost(string, tag = "6")]
    pub direction: String,
    /// Raw RTP payload bytes (codec-specific; for PCMU this is
    /// 160 bytes at 20 ms).
    #[prost(bytes = "vec", tag = "7")]
    pub payload: Vec<u8>,
}

/// Wire format identifier. Appears on the plugin manifest as
/// `wire_format = "proto" | "flatbuffers"` and is the dimension
/// the [`WireFormat`] trait selects over.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum WireFormatKind {
    /// Protobuf (prost). The pre-5.2 default; every plugin that
    /// omits the manifest field ends up here.
    #[default]
    Proto,
    /// Flat zero-copy layout (slice 5.2 / P18). Requires the
    /// `flatbuffers` Cargo feature on the reader side.
    Flatbuffers,
}

impl WireFormatKind {
    /// Parse a manifest token case-insensitively. Unknown tokens
    /// return `None` so the plugin loader can surface a targeted
    /// error rather than silently falling back to a default.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let lower = s.to_ascii_lowercase();
        match lower.as_str() {
            "proto" | "protobuf" | "prost" => Some(Self::Proto),
            "flatbuffers" | "flatbuf" | "fbs" => Some(Self::Flatbuffers),
            _ => None,
        }
    }

    /// Wire-format token emitted on round-trip serialization.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proto => "proto",
            Self::Flatbuffers => "flatbuffers",
        }
    }
}

/// Error variants returned by [`WireFormat::decode_rtp_frame`] /
/// [`WireFormat::decode_envelope`]. Flat enum so `no_std` consumers
/// don't need to pull a boxed-error trait.
#[derive(Debug)]
pub enum WireFormatError {
    /// The byte stream didn't match the declared format's invariants.
    Malformed(&'static str),
}

impl core::fmt::Display for WireFormatError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Malformed(m) => write!(f, "wire format: malformed: {m}"),
        }
    }
}

#[cfg(feature = "flatbuffers")]
impl std::error::Error for WireFormatError {}

/// Encoder / decoder pair for the plugin wire. The trait exists so
/// the engine can pick between `proto` (today's default, schema-
/// evolution-friendly) and `flatbuffers` (zero-copy reads, ≥2×
/// throughput on the `RtpFrame` hot path) per plugin without the
/// caller caring which is which.
///
/// Both impls are symmetric — an `Envelope` or `RtpFrame` encoded
/// with one can only be decoded with the same impl. The plugin
/// manifest's `wire_format` field is the coordination point.
pub trait WireFormat {
    /// Which format this impl implements.
    fn kind(&self) -> WireFormatKind;

    /// Serialize an [`Envelope`] to a byte buffer.
    fn encode_envelope(&self, envelope: &Envelope) -> Vec<u8>;

    /// Deserialize an [`Envelope`] from `bytes`. Returns
    /// [`WireFormatError::Malformed`] when the bytes don't fit the
    /// declared format.
    fn decode_envelope(&self, bytes: &[u8]) -> Result<Envelope, WireFormatError>;

    /// Serialize an [`RtpFrame`] to a byte buffer. Hot path: called
    /// ~50×/sec/leg, so impls keep the per-call allocation count
    /// as low as the format allows.
    fn encode_rtp_frame(&self, frame: &RtpFrame) -> Vec<u8>;

    /// Deserialize an [`RtpFrame`] from `bytes`. Hot path on the
    /// plugin side too.
    fn decode_rtp_frame(&self, bytes: &[u8]) -> Result<RtpFrame, WireFormatError>;
}

/// Protobuf wire format — uses the prost derives already on
/// [`Envelope`] and [`RtpFrame`]. Pre-5.2 default; every plugin
/// that omits `wire_format` in its manifest ends up here.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProtoWireFormat;

impl WireFormat for ProtoWireFormat {
    fn kind(&self) -> WireFormatKind {
        WireFormatKind::Proto
    }

    fn encode_envelope(&self, envelope: &Envelope) -> Vec<u8> {
        envelope.encode_to_vec()
    }

    fn decode_envelope(&self, bytes: &[u8]) -> Result<Envelope, WireFormatError> {
        Envelope::decode(bytes).map_err(|_| WireFormatError::Malformed("envelope protobuf decode"))
    }

    fn encode_rtp_frame(&self, frame: &RtpFrame) -> Vec<u8> {
        frame.encode_to_vec()
    }

    fn decode_rtp_frame(&self, bytes: &[u8]) -> Result<RtpFrame, WireFormatError> {
        RtpFrame::decode(bytes).map_err(|_| WireFormatError::Malformed("rtp frame protobuf decode"))
    }
}

#[cfg(feature = "flatbuffers")]
pub use flatbuffers_io::FlatbuffersWireFormat;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let req = Request {
            id: 42,
            method: "synthesize".into(),
            params: r#"{"text":"hi"}"#.into(),
        };
        let bytes = encode(&req);
        let decoded: Request = decode(&bytes).unwrap();
        assert_eq!(decoded, req);
    }

    #[test]
    fn response_success_round_trips() {
        let resp = Response {
            id: 42,
            result: r#"{"audio_base64":"AAA="}"#.into(),
            error_message: String::new(),
            error_code: 0,
        };
        let bytes = encode(&resp);
        let decoded: Response = decode(&bytes).unwrap();
        assert_eq!(decoded, resp);
    }

    #[test]
    fn response_error_round_trips() {
        let resp = Response {
            id: 7,
            result: String::new(),
            error_message: "method not found".into(),
            error_code: -32601,
        };
        let bytes = encode(&resp);
        let decoded: Response = decode(&bytes).unwrap();
        assert_eq!(decoded, resp);
    }

    #[test]
    fn notification_round_trips() {
        let n = Notification {
            method: "emit_partial".into(),
            params: r#"{"text":"ho"}"#.into(),
        };
        let bytes = encode(&n);
        let decoded: Notification = decode(&bytes).unwrap();
        assert_eq!(decoded, n);
    }

    #[test]
    fn envelope_carries_each_variant() {
        let req = Request {
            id: 1,
            method: "m".into(),
            params: String::new(),
        };
        let env = Envelope {
            kind: Some(envelope::Kind::Request(req.clone())),
        };
        let bytes = encode(&env);
        let decoded: Envelope = decode(&bytes).unwrap();
        match decoded.kind {
            Some(envelope::Kind::Request(r)) => assert_eq!(r, req),
            other => panic!("expected Request, got {other:?}"),
        }
    }

    #[test]
    fn envelope_empty_decodes_cleanly_with_no_kind_set() {
        // Peers that see an envelope with no kind should treat it as
        // malformed at the application layer, but decode itself must
        // succeed — prost treats missing oneofs as `None`.
        let env = Envelope { kind: None };
        let bytes = encode(&env);
        let decoded: Envelope = decode(&bytes).unwrap();
        assert!(decoded.kind.is_none());
    }
}
