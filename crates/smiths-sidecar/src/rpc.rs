//! JSON-RPC 2.0 types used on the sidecar wire.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Engine-side request frame (engine → plugin).
#[derive(Debug, Clone, Serialize)]
pub struct RpcRequest<'a> {
    /// Always `"2.0"`.
    pub jsonrpc: &'a str,
    /// Monotonic request id.
    pub id: u64,
    /// Method name, e.g. `"describe_capabilities"`.
    pub method: &'a str,
    /// Arbitrary JSON parameters.
    pub params: Value,
}

/// Frame received from the plugin. May be a response (with `id`) or a
/// notification (without `id`) — notifications are plugin-initiated
/// events, e.g. streaming TTS audio chunks.
#[derive(Debug, Clone, Deserialize)]
pub struct RpcResponse {
    /// Request id this response correlates to. `None` = notification.
    #[serde(default)]
    pub id: Option<u64>,
    /// Successful result payload. Mutually exclusive with `error`.
    #[serde(default)]
    pub result: Option<Value>,
    /// Error frame. Mutually exclusive with `result`.
    #[serde(default)]
    pub error: Option<RpcError>,
    /// Method name — present on notifications only.
    #[serde(default)]
    pub method: Option<String>,
    /// Params on notifications.
    #[serde(default)]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 error sub-frame.
#[derive(Debug, Clone, Deserialize)]
pub struct RpcError {
    /// Error code (JSON-RPC 2.0 conventions plus `-32001..` app range).
    pub code: i64,
    /// Human-readable message.
    pub message: String,
    /// Optional structured diagnostic payload.
    #[serde(default)]
    pub data: Option<Value>,
}
