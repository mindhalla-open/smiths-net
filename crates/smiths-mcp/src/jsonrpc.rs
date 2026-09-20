//! JSON-RPC 2.0 framing shared by every adapter that speaks it
//! (MCP stdio, MCP HTTP, A2A).
//!
//! One place for the error-code table and the response envelopes so
//! adapters cannot drift apart on what `not found` or `forbidden`
//! means on the wire.

use serde_json::{Map, Value, json};

use crate::tool::ToolError;

/// Malformed JSON.
pub const ERR_PARSE: i64 = -32700;
/// Structurally invalid request (missing `method`, oversize frame).
pub const ERR_INVALID_REQUEST: i64 = -32600;
/// Unknown JSON-RPC method (or unknown resource URI).
pub const ERR_METHOD_NOT_FOUND: i64 = -32601;
/// Missing or malformed params / tool arguments.
pub const ERR_INVALID_PARAMS: i64 = -32602;
/// Unexpected server-side failure.
pub const ERR_INTERNAL: i64 = -32603;
/// Application code (MCP convention `-32000..-32099`): unknown tool
/// or unknown referent (call id, plugin,...).
pub const ERR_TOOL_NOT_FOUND: i64 = -32001;
/// Application code: rate limited or not authorized.
pub const ERR_FORBIDDEN: i64 = -32002;
/// Application code: referent exists but is in a state that
/// rejects the operation (e.g. a call without a media leg).
pub const ERR_CONFLICT: i64 = -32003;

/// Build a JSON-RPC error frame.
#[must_use]
pub fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

/// Build a JSON-RPC success frame.
#[must_use]
pub fn success_response(id: &Value, result: Value) -> Value {
    let mut frame = Map::with_capacity(3);
    frame.insert("jsonrpc".into(), Value::from("2.0"));
    frame.insert("id".into(), id.clone());
    frame.insert("result".into(), result);
    Value::Object(frame)
}

/// JSON-RPC error code paired with a [`ToolError`] variant.
#[must_use]
pub fn tool_error_code(err: &ToolError) -> i64 {
    match err {
        ToolError::InvalidArguments(_) => ERR_INVALID_PARAMS,
        ToolError::NotFound(_) => ERR_TOOL_NOT_FOUND,
        ToolError::Forbidden(_) => ERR_FORBIDDEN,
        ToolError::Conflict(_) => ERR_CONFLICT,
        ToolError::Internal(_) => ERR_INTERNAL,
    }
}

/// `(code, message)` pair for a [`ToolError`], ready for
/// [`error_response`].
#[must_use]
pub fn map_tool_error(err: &ToolError) -> (i64, String) {
    (tool_error_code(err), err.to_string())
}

/// Parsed shape of one incoming JSON-RPC message.
#[derive(Debug)]
pub struct Request {
    /// `id` as sent (`Null` when absent).
    pub id: Value,
    /// `true` when the message carried no `id` — a notification that
    /// must never be answered, even on error.
    pub is_notification: bool,
    /// `method` name.
    pub method: String,
    /// `params` as sent (`Null` when absent).
    pub params: Value,
}

impl Request {
    /// Pull the request fields out of a parsed JSON value. Returns
    /// the error frame to send back when `method` is missing.
    pub fn from_value(v: &Value) -> Result<Self, Value> {
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        let is_notification = v.get("id").is_none();
        let Some(method) = v.get("method").and_then(Value::as_str) else {
            return Err(error_response(&id, ERR_INVALID_REQUEST, "missing method"));
        };
        Ok(Self {
            id,
            is_notification,
            method: method.to_owned(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_parses_id_method_params() {
        let v = json!({"jsonrpc": "2.0", "id": 7, "method": "ping", "params": {"a": 1}});
        let req = Request::from_value(&v).unwrap();
        assert_eq!(req.id, 7);
        assert!(!req.is_notification);
        assert_eq!(req.method, "ping");
        assert_eq!(req.params["a"], 1);
    }

    #[test]
    fn request_without_id_is_notification() {
        let v = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let req = Request::from_value(&v).unwrap();
        assert!(req.is_notification);
        assert!(req.params.is_null());
    }

    #[test]
    fn request_without_method_yields_invalid_request_frame() {
        let v = json!({"jsonrpc": "2.0", "id": 1});
        let err = Request::from_value(&v).unwrap_err();
        assert_eq!(err["error"]["code"], ERR_INVALID_REQUEST);
        assert_eq!(err["id"], 1);
    }

    #[test]
    fn every_tool_error_maps_to_a_distinct_app_code() {
        let codes = [
            tool_error_code(&ToolError::InvalidArguments("x".into())),
            tool_error_code(&ToolError::NotFound("x".into())),
            tool_error_code(&ToolError::Forbidden("x".into())),
            tool_error_code(&ToolError::Conflict("x".into())),
            tool_error_code(&ToolError::Internal("x".into())),
        ];
        for (i, a) in codes.iter().enumerate() {
            for (j, b) in codes.iter().enumerate() {
                assert!(i == j || a != b, "codes collide: {a} vs {b}");
            }
        }
        assert_eq!(codes[1], ERR_TOOL_NOT_FOUND);
        assert_eq!(codes[2], ERR_FORBIDDEN);
        assert_eq!(codes[3], ERR_CONFLICT);
    }
}
