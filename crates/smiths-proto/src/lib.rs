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

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use prost::Message;

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
