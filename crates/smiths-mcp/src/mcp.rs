//! MCP (Model Context Protocol) server over stdio.
//!
//! Implements the subset of the MCP spec our control plane needs today:
//! `initialize`, `tools/list`, `tools/call`, plus **server-pushed
//! notifications** for SIP dialog lifecycle (`notifications/call/created`,
//! `notifications/call/terminated`).
//!
//! Framing is newline-delimited JSON-RPC 2.0: each line of stdin is one
//! request, each line of stdout is one response or notification. All
//! logging goes to `stderr` so it doesn't corrupt the wire.
//!
//! Not yet:
//! * resources / prompts
//! * authentication (token or OAuth)
//! * per-tool rate limiting
//!
//! Add them as their own request handlers when we need them — the
//! dispatch loop is a plain `match` on `method`.

use std::sync::Arc;

use serde_json::{Map, Value, json};
use smiths_core::{Event, EventBus, SipEvent};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::tool::{ToolContext, ToolError, ToolRegistry};

/// Protocol version we advertise in `initialize`. Matches the MCP
/// `2024-11-05` revision subset we implement.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Run the MCP server against `stdin` / `stdout`. Returns when stdin
/// EOFs or `cancel` fires.
///
/// The server multiplexes two writers onto stdout: request handlers
/// (one response per stdin frame) and bus-driven notifications
/// (fire-and-forget). They share a single `select!` loop so ordering
/// is serial and no mutex is required on stdout.
pub async fn run_stdio(
    registry: Arc<ToolRegistry>,
    ctx: ToolContext,
    bus: EventBus,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    let mut bus_rx = bus.subscribe();
    info!(
        tools = registry.len(),
        protocol = PROTOCOL_VERSION,
        "MCP stdio server ready"
    );

    let mut line = String::new();
    loop {
        line.clear();
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            n = reader.read_line(&mut line) => {
                match n {
                    Ok(0) => break, // EOF
                    Ok(_) => {}
                    Err(e) => {
                        warn!(?e, "stdin read error");
                        return Err(e);
                    }
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let response = handle_frame(trimmed, &registry, &ctx).await;
                if let Some(resp) = response {
                    write_frame(&mut stdout, &resp).await?;
                }
            }
            event = bus_rx.recv() => match event {
                Ok(ev) => {
                    if let Some(notif) = event_to_notification(&ev) {
                        write_frame(&mut stdout, &notif).await?;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    debug!("MCP stdio bus lagged: {n} events dropped");
                }
                Err(RecvError::Closed) => {
                    debug!("MCP stdio bus closed");
                }
            },
        }
    }
    info!("MCP stdio server stopped");
    Ok(())
}

/// Serialize one JSON-RPC frame and write it as a single line to stdout.
async fn write_frame(stdout: &mut tokio::io::Stdout, frame: &Value) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(frame).unwrap_or_default();
    buf.push(b'\n');
    stdout.write_all(&buf).await?;
    stdout.flush().await
}

/// Translate a bus event into an MCP notification frame. `None` for
/// events we don't expose (keeps the wire quiet and forward-compatible).
fn event_to_notification(event: &Event) -> Option<Value> {
    match event {
        Event::Sip(SipEvent::DialogCreated { call_id }) => Some(json!({
            "jsonrpc": "2.0",
            "method": "notifications/call/created",
            "params": { "call_id": call_id },
        })),
        Event::Sip(SipEvent::DialogTerminated { call_id }) => Some(json!({
            "jsonrpc": "2.0",
            "method": "notifications/call/terminated",
            "params": { "call_id": call_id },
        })),
        _ => None,
    }
}

/// Parse one JSON-RPC request line and produce an optional response.
/// Notifications (no `id`) return `None`.
async fn handle_frame(line: &str, registry: &ToolRegistry, ctx: &ToolContext) -> Option<Value> {
    // Parse loosely — we emit parse-error with id=null if malformed.
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(error_response(
                &Value::Null,
                ERR_PARSE,
                &format!("parse error: {e}"),
            ));
        }
    };

    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = req.get("method").and_then(Value::as_str) else {
        return Some(error_response(&id, ERR_INVALID_REQUEST, "missing method"));
    };
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    let is_notification = req.get("id").is_none();
    let result = dispatch(method, params, registry, ctx).await;

    if is_notification {
        // Per JSON-RPC 2.0: notifications never get a response, even
        // on error.
        return None;
    }
    match result {
        Ok(value) => Some(json!({ "jsonrpc": "2.0", "id": id, "result": value })),
        Err((code, message)) => Some(error_response(&id, code, &message)),
    }
}

async fn dispatch(
    method: &str,
    params: Value,
    registry: &ToolRegistry,
    ctx: &ToolContext,
) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(initialize_response()),
        "initialized" | "notifications/initialized" | "shutdown" => Ok(Value::Null),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list_response(registry)),
        "tools/call" => tools_call(params, registry, ctx).await,
        other => Err((ERR_METHOD_NOT_FOUND, format!("unknown method: {other}"))),
    }
}

fn initialize_response() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "serverInfo": {
            "name": "smiths-net",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "capabilities": {
            "tools": { "listChanged": false },
        }
    })
}

fn tools_list_response(registry: &ToolRegistry) -> Value {
    let tools: Vec<_> = registry
        .iter()
        .map(|t| {
            json!({
                "name": t.name(),
                "description": t.description(),
                "inputSchema": t.input_schema(),
            })
        })
        .collect();
    json!({ "tools": tools })
}

async fn tools_call(
    params: Value,
    registry: &ToolRegistry,
    ctx: &ToolContext,
) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((ERR_INVALID_PARAMS, "missing `name`".to_owned()))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::default()));

    let Some(tool) = registry.get(name) else {
        return Err((ERR_METHOD_NOT_FOUND, format!("unknown tool: {name}")));
    };

    match tool.call(args, ctx).await {
        Ok(output) => {
            // MCP `tools/call` response wraps output as a content
            // array; we always return a single JSON text block.
            let text = serde_json::to_string(&output).unwrap_or_default();
            Ok(json!({
                "content": [ { "type": "text", "text": text } ],
                "isError": false,
                "structuredContent": output,
            }))
        }
        Err(err) => {
            let (code, _) = tool_error_code(&err);
            let message = err.to_string();
            debug!(%name, %message, "tool returned error");
            // MCP wraps tool errors in the success envelope with
            // `isError: true`, rather than JSON-RPC error frames.
            // That's per the spec.
            let _ = code; // currently unused; reserved for audit logging
            Ok(json!({
                "content": [ { "type": "text", "text": message } ],
                "isError": true,
            }))
        }
    }
}

fn tool_error_code(err: &ToolError) -> (i64, &'static str) {
    match err {
        ToolError::InvalidArguments(_) => (ERR_INVALID_PARAMS, "invalid arguments"),
        ToolError::NotFound(_) => (ERR_TOOL_NOT_FOUND, "not found"),
        ToolError::Forbidden(_) => (ERR_FORBIDDEN, "forbidden"),
        ToolError::Internal(_) => (ERR_INTERNAL, "internal"),
    }
}

fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

// JSON-RPC 2.0 standard error codes, extended with MCP-style app codes.
const ERR_PARSE: i64 = -32700;
const ERR_INVALID_REQUEST: i64 = -32600;
const ERR_METHOD_NOT_FOUND: i64 = -32601;
const ERR_INVALID_PARAMS: i64 = -32602;
const ERR_INTERNAL: i64 = -32603;
// App-range codes (MCP convention: -32000..-32099).
const ERR_TOOL_NOT_FOUND: i64 = -32001;
const ERR_FORBIDDEN: i64 = -32002;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ControlState;
    use smiths_core::EventBus;

    fn ctx() -> (ToolContext, CancellationToken) {
        let bus = EventBus::new(8);
        let cancel = CancellationToken::new();
        let (state, _task) = ControlState::spawn(&bus, cancel.clone());
        (ToolContext::new(state), cancel)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn initialize_returns_server_info() {
        let reg = crate::tools::builtin_registry();
        let (c, _cancel) = ctx();
        let frame = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let resp = handle_frame(frame, &reg, &c).await.unwrap();
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["serverInfo"]["name"], "smiths-net");
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tools_list_contains_builtins() {
        let reg = crate::tools::builtin_registry();
        let (c, _cancel) = ctx();
        let frame = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let resp = handle_frame(frame, &reg, &c).await.unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"list_calls"));
        assert!(names.contains(&"get_call_status"));
        assert!(names.contains(&"health"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tools_call_health_ok() {
        let reg = crate::tools::builtin_registry();
        let (c, _cancel) = ctx();
        let frame = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"health","arguments":{}}}"#;
        let resp = handle_frame(frame, &reg, &c).await.unwrap();
        assert_eq!(resp["result"]["isError"], false);
        assert_eq!(resp["result"]["structuredContent"]["status"], "ok");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_method_returns_error() {
        let reg = crate::tools::builtin_registry();
        let (c, _cancel) = ctx();
        let frame = r#"{"jsonrpc":"2.0","id":4,"method":"does/not/exist"}"#;
        let resp = handle_frame(frame, &reg, &c).await.unwrap();
        assert_eq!(resp["error"]["code"], ERR_METHOD_NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn notification_produces_no_response() {
        let reg = crate::tools::builtin_registry();
        let (c, _cancel) = ctx();
        // No `id` — this is a notification.
        let frame = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let resp = handle_frame(frame, &reg, &c).await;
        assert!(resp.is_none());
    }
}
