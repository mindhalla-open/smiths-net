//! How a sidecar's stdio is framed.
//!
//! JSON-RPC over newline-delimited stdio is the default and what every
//! example plugin speaks: one JSON object per line, no dependencies,
//! no schema compiler. A plugin that moves enough traffic for the
//! per-frame JSON cost to matter can opt into a length-prefixed
//! binary frame instead by setting `wire_format = "binary"` in its
//! `plugin.toml`.
//!
//! Both encodings carry the same logical messages — a request with an
//! id, a response correlating to one, and an id-less notification —
//! so the supervisor's timeouts, frame cap, restart policy and
//! notification bridge behave identically either way.
//!
//! ## Binary frame
//!
//! ```text
//! 4 bytes  big-endian u32 length of the body
//! N bytes  smiths_proto::Envelope in the fixed-offset layout
//! ```
//!
//! The declared length is checked against the frame cap *before* any
//! allocation, so a hostile or broken child cannot make the engine
//! reserve an arbitrary buffer.

use serde_json::Value;
use smiths_proto::{Envelope, FixedFrameWireFormat, WireFormat as _, envelope};

use crate::rpc::{RpcError, RpcResponse};

/// Bytes of length prefix on a binary frame.
pub(crate) const LENGTH_PREFIX: usize = 4;

/// Stdio encoding for one plugin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WireFormat {
    /// Newline-delimited JSON-RPC 2.0. The default: a plugin is one
    /// file with no dependencies.
    #[default]
    Json,
    /// Length-prefixed [`smiths_proto::Envelope`] frames.
    Binary,
}

impl WireFormat {
    /// Name as it appears in `plugin.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Binary => "binary",
        }
    }

    /// Every accepted manifest value, for error messages.
    #[must_use]
    pub const fn accepted() -> &'static [&'static str] {
        &["json", "binary"]
    }
}

/// A frame could not be encoded or decoded.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The declared body length exceeds the configured cap.
    #[error("frame declares {declared} bytes, cap is {cap}")]
    TooLarge {
        /// Length the child claimed.
        declared: usize,
        /// Configured maximum.
        cap: usize,
    },
    /// The body did not decode as an envelope.
    #[error("malformed binary frame: {0}")]
    Malformed(String),
    /// The envelope carried no variant, so there is nothing to act on.
    #[error("binary frame carried no request, response or notification")]
    Empty,
}

/// Encode an outbound request.
///
/// `params` is carried as its JSON text in both encodings, so a plugin
/// parses the same document either way.
pub(crate) fn encode_request(fmt: WireFormat, id: u64, method: &str, params: &Value) -> Vec<u8> {
    match fmt {
        WireFormat::Json => {
            let frame = crate::rpc::RpcRequest {
                jsonrpc: "2.0",
                id,
                method,
                params: params.clone(),
            };
            let mut buf = serde_json::to_vec(&frame).unwrap_or_else(|_| {
                // `RpcRequest` is plain data; a failure here would be
                // a serde bug rather than bad input.
                br#"{"jsonrpc":"2.0","id":0,"method":"","params":null}"#.to_vec()
            });
            buf.push(b'\n');
            buf
        }
        WireFormat::Binary => {
            let env = Envelope {
                kind: Some(envelope::Kind::Request(smiths_proto::Request {
                    id,
                    method: method.to_owned(),
                    params: params.to_string(),
                })),
            };
            let body = FixedFrameWireFormat.encode_envelope(&env);
            let mut buf = Vec::with_capacity(LENGTH_PREFIX + body.len());
            let len = u32::try_from(body.len()).unwrap_or(u32::MAX);
            buf.extend_from_slice(&len.to_be_bytes());
            buf.extend_from_slice(&body);
            buf
        }
    }
}

/// Decode one binary frame body into the response shape the
/// supervisor already dispatches on.
///
/// # Errors
///
/// [`FrameError::Malformed`] when the bytes are not an envelope,
/// [`FrameError::Empty`] when the envelope has no variant set.
pub(crate) fn decode_binary_body(body: &[u8]) -> Result<RpcResponse, FrameError> {
    let env = FixedFrameWireFormat
        .decode_envelope(body)
        .map_err(|e| FrameError::Malformed(e.to_string()))?;
    let kind = env.kind.ok_or(FrameError::Empty)?;
    Ok(match kind {
        envelope::Kind::Response(r) => {
            let error = if r.error_message.is_empty() {
                None
            } else {
                Some(RpcError {
                    code: i64::from(r.error_code),
                    message: r.error_message,
                    data: None,
                })
            };
            let result = if r.result.is_empty() {
                None
            } else {
                // A plugin is free to send any UTF-8; when it is not
                // JSON, hand it on as a JSON string rather than
                // failing the whole frame.
                Some(match serde_json::from_str(&r.result) {
                    Ok(v) => v,
                    Err(_) => Value::String(r.result),
                })
            };
            RpcResponse {
                id: Some(r.id),
                result,
                error,
                method: None,
                params: None,
            }
        }
        envelope::Kind::Notification(n) => RpcResponse {
            id: None,
            result: None,
            error: None,
            params: Some(match serde_json::from_str(&n.params) {
                Ok(v) => v,
                Err(_) => Value::String(n.params),
            }),
            method: Some(n.method),
        },
        // A child that sends a *request* is talking a protocol the
        // engine does not offer; treat it as malformed rather than
        // silently ignoring it.
        envelope::Kind::Request(r) => {
            return Err(FrameError::Malformed(format!(
                "child sent a request (`{}`); sidecars only reply and notify",
                r.method
            )));
        }
    })
}

/// Validate a declared body length against the frame cap.
///
/// # Errors
///
/// [`FrameError::TooLarge`] when the child asked for more than `cap`.
pub(crate) fn check_declared_len(declared: usize, cap: usize) -> Result<(), FrameError> {
    if declared > cap {
        return Err(FrameError::TooLarge { declared, cap });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_request_is_one_newline_terminated_object() {
        let bytes = encode_request(WireFormat::Json, 7, "invoke", &json!({"a": 1}));
        assert_eq!(bytes.last(), Some(&b'\n'));
        let v: Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).expect("json");
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "invoke");
        assert_eq!(v["params"]["a"], 1);
    }

    #[test]
    fn binary_request_is_length_prefixed_and_round_trips() {
        let bytes = encode_request(WireFormat::Binary, 9, "describe", &json!({"b": 2}));
        let declared =
            u32::from_be_bytes(bytes[..LENGTH_PREFIX].try_into().expect("prefix")) as usize;
        assert_eq!(declared, bytes.len() - LENGTH_PREFIX, "prefix matches body");

        let env = FixedFrameWireFormat
            .decode_envelope(&bytes[LENGTH_PREFIX..])
            .expect("decode");
        match env.kind.expect("kind") {
            envelope::Kind::Request(r) => {
                assert_eq!(r.id, 9);
                assert_eq!(r.method, "describe");
                let params: Value = serde_json::from_str(&r.params).expect("params json");
                assert_eq!(params["b"], 2);
            }
            _ => panic!("expected a request"),
        }
    }

    #[test]
    fn binary_response_decodes_into_the_json_shape() {
        let env = Envelope {
            kind: Some(envelope::Kind::Response(smiths_proto::Response {
                id: 4,
                result: r#"{"ok":true}"#.into(),
                error_message: String::new(),
                error_code: 0,
            })),
        };
        let body = FixedFrameWireFormat.encode_envelope(&env);
        let resp = decode_binary_body(&body).expect("decode");
        assert_eq!(resp.id, Some(4));
        assert_eq!(resp.result.expect("result")["ok"], true);
        assert!(resp.error.is_none());
    }

    #[test]
    fn binary_error_response_carries_code_and_message() {
        let env = Envelope {
            kind: Some(envelope::Kind::Response(smiths_proto::Response {
                id: 5,
                result: String::new(),
                error_message: "boom".into(),
                error_code: -32601,
            })),
        };
        let body = FixedFrameWireFormat.encode_envelope(&env);
        let resp = decode_binary_body(&body).expect("decode");
        let err = resp.error.expect("error");
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "boom");
    }

    #[test]
    fn binary_notification_has_no_id() {
        let env = Envelope {
            kind: Some(envelope::Kind::Notification(smiths_proto::Notification {
                method: "partial".into(),
                params: r#"{"text":"hel"}"#.into(),
            })),
        };
        let body = FixedFrameWireFormat.encode_envelope(&env);
        let resp = decode_binary_body(&body).expect("decode");
        assert!(resp.id.is_none());
        assert_eq!(resp.method.as_deref(), Some("partial"));
        assert_eq!(resp.params.expect("params")["text"], "hel");
    }

    #[test]
    fn truncated_binary_frame_is_an_error_not_a_panic() {
        let env = Envelope {
            kind: Some(envelope::Kind::Response(smiths_proto::Response {
                id: 1,
                result: "{}".into(),
                error_message: String::new(),
                error_code: 0,
            })),
        };
        let body = FixedFrameWireFormat.encode_envelope(&env);
        for cut in [0, 1, body.len() / 2, body.len() - 1] {
            let err = decode_binary_body(&body[..cut]);
            assert!(err.is_err(), "truncating to {cut} bytes must not decode");
        }
    }

    #[test]
    fn oversized_declared_length_is_rejected_before_allocating() {
        let err = check_declared_len(64 * 1024 * 1024, 16 * 1024 * 1024)
            .expect_err("an oversized frame must be refused");
        assert!(matches!(err, FrameError::TooLarge { .. }), "{err}");
        assert!(check_declared_len(1024, 16 * 1024 * 1024).is_ok());
    }

    #[test]
    fn a_child_request_is_refused() {
        let env = Envelope {
            kind: Some(envelope::Kind::Request(smiths_proto::Request {
                id: 1,
                method: "surprise".into(),
                params: "{}".into(),
            })),
        };
        let body = FixedFrameWireFormat.encode_envelope(&env);
        let err = decode_binary_body(&body).expect_err("children may not send requests");
        assert!(err.to_string().contains("surprise"), "{err}");
    }

    #[test]
    fn wire_format_names_round_trip_through_serde() {
        assert_eq!(
            serde_json::from_str::<WireFormat>("\"binary\"").expect("binary"),
            WireFormat::Binary
        );
        assert_eq!(WireFormat::default(), WireFormat::Json);
        assert_eq!(WireFormat::Binary.as_str(), "binary");
    }
}
