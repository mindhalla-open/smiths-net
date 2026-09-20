//! MCP (Model Context Protocol) server over stdio.
//!
//! Implements the subset of the MCP spec our control plane needs:
//! `initialize` (with protocol-version negotiation), `ping`,
//! `tools/list`, `tools/call`, `resources/list`, `resources/read`,
//! plus **server-pushed notifications** for SIP dialog lifecycle
//! (`notifications/call/created`, `notifications/call/terminated`)
//! and plugin notifications (`notifications/plugin/<method>`).
//!
//! Framing is newline-delimited JSON-RPC 2.0: each line of stdin is
//! one request, each line of stdout is one response or notification.
//! A line longer than [`MAX_FRAME_BYTES`] is discarded and answered
//! with a JSON-RPC `invalid request` error so a runaway client cannot
//! grow memory without bound. All logging goes to `stderr` so it
//! doesn't corrupt the wire.
//!
//! Method routing, rate limiting, argument-schema validation, audit
//! and metrics all live on [`ProtocolDispatch`] — the same pipeline
//! the HTTP adapters use. Prompts and resource subscriptions are not
//! implemented; `initialize` advertises exactly what is.

use std::sync::Arc;

use serde_json::{Value, json};
use smiths_core::{Event, EventBus, Metrics, PluginEvent, SipEvent};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::control_protocol::ProtocolDispatch;
use crate::jsonrpc::{ERR_INVALID_REQUEST, error_response};
use crate::rate_limit::RateLimiter;
use crate::resource::ResourceRegistry;
use crate::tool::{ToolContext, ToolRegistry};

/// Actor label recorded in audit events for this adapter.
const ACTOR: &str = "mcp-stdio";

/// Protocol version we answer with when the client asks for one we
/// don't support (or none at all). Streamable HTTP is the transport
/// this revision introduced, so it is the natural default.
pub const PROTOCOL_VERSION: &str = "2025-03-26";

/// Every protocol revision `initialize` accepts. A client naming one
/// of these gets it echoed back; anything else gets
/// [`PROTOCOL_VERSION`].
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

/// Largest single JSON-RPC frame (one line) the stdio server accepts.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Pick the protocol version to answer `initialize` with.
#[must_use]
pub fn negotiate_protocol_version(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|r| SUPPORTED_PROTOCOL_VERSIONS.iter().find(|v| **v == r))
        .copied()
        .unwrap_or(PROTOCOL_VERSION)
}

/// `initialize` result: negotiated version, server info, and the
/// capabilities this server actually implements.
#[must_use]
pub fn initialize_response(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    json!({
        "protocolVersion": negotiate_protocol_version(requested),
        "serverInfo": {
            "name": "smiths-net",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "capabilities": {
            "tools":     { "listChanged": false },
            "resources": { "listChanged": false, "subscribe": false },
        }
    })
}

/// Run the MCP server against `stdin` / `stdout`. Returns when stdin
/// EOFs or `cancel` fires.
pub async fn run_stdio(
    registry: Arc<ToolRegistry>,
    resources: Arc<ResourceRegistry>,
    rate_limiter: Arc<RateLimiter>,
    metrics: Arc<Metrics>,
    ctx: ToolContext,
    bus: EventBus,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let dispatch = ProtocolDispatch::new(registry, resources, rate_limiter, metrics, ctx);
    run_io(
        tokio::io::stdin(),
        tokio::io::stdout(),
        dispatch,
        bus,
        cancel,
    )
    .await
}

/// Run the MCP server over an arbitrary byte stream pair. This is
/// what [`run_stdio`] wraps around stdin/stdout; tests drive it with
/// an in-memory duplex.
///
/// The loop multiplexes two writers onto `writer`: request handlers
/// (one response per input frame) and bus-driven notifications
/// (fire-and-forget). They share a single `select!` loop so ordering
/// is serial and no mutex is required on the output.
pub async fn run_io<R, W>(
    reader: R,
    mut writer: W,
    dispatch: ProtocolDispatch,
    bus: EventBus,
    cancel: CancellationToken,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(reader);
    let mut frames = FrameReader::new(MAX_FRAME_BYTES);
    let mut bus_rx = bus.subscribe();
    info!(
        tools = dispatch.registry.len(),
        resources = dispatch.resources.len(),
        protocol = PROTOCOL_VERSION,
        "MCP stdio server ready"
    );

    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            frame = frames.next_frame(&mut reader) => {
                let response = match frame {
                    Ok(Frame::Eof) => break,
                    Ok(Frame::TooLong) => Some(error_response(
                        &Value::Null,
                        ERR_INVALID_REQUEST,
                        &format!("frame exceeds {MAX_FRAME_BYTES} bytes"),
                    )),
                    Ok(Frame::Line(line)) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        dispatch.handle_frame(ACTOR, None, trimmed).await
                    }
                    Err(e) => {
                        warn!(?e, "stdin read error");
                        return Err(e);
                    }
                };
                if let Some(resp) = response {
                    write_frame(&mut writer, &resp).await?;
                }
            }
            event = bus_rx.recv() => match event {
                Ok(ev) => {
                    if let Some(notif) = event_to_notification(&ev) {
                        write_frame(&mut writer, &notif).await?;
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

/// One unit read off the input stream.
enum Frame {
    /// A complete line (without its trailing newline).
    Line(String),
    /// A line longer than the cap; its bytes were discarded.
    TooLong,
    /// Input closed.
    Eof,
}

/// Bounded line reader. All state lives on the struct so the
/// `next_frame` future is cancellation-safe: a bus event winning the
/// `select!` drops the future mid-read without losing bytes already
/// pulled off the stream.
struct FrameReader {
    max: usize,
    buf: Vec<u8>,
    discarding: bool,
}

impl FrameReader {
    fn new(max: usize) -> Self {
        Self {
            max,
            buf: Vec::new(),
            discarding: false,
        }
    }

    async fn next_frame<R: AsyncBufRead + Unpin>(
        &mut self,
        reader: &mut R,
    ) -> std::io::Result<Frame> {
        loop {
            let (newline_at, chunk_len) = {
                let available = reader.fill_buf().await?;
                if available.is_empty() {
                    return Ok(self.finish_at_eof());
                }
                let newline_at = available.iter().position(|&b| b == b'\n');
                let take = newline_at.unwrap_or(available.len());
                if !self.discarding {
                    self.buf.extend_from_slice(&available[..take]);
                    if self.buf.len() > self.max {
                        self.discarding = true;
                        self.buf.clear();
                    }
                }
                (newline_at, available.len())
            };
            match newline_at {
                Some(i) => {
                    reader.consume(i + 1);
                    return Ok(self.finish_line());
                }
                None => reader.consume(chunk_len),
            }
        }
    }

    fn finish_line(&mut self) -> Frame {
        if std::mem::take(&mut self.discarding) {
            self.buf.clear();
            return Frame::TooLong;
        }
        let bytes = std::mem::take(&mut self.buf);
        Frame::Line(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn finish_at_eof(&mut self) -> Frame {
        if self.discarding || !self.buf.is_empty() {
            // A trailing line without a newline still counts.
            self.finish_line()
        } else {
            Frame::Eof
        }
    }
}

/// Serialize one JSON-RPC frame and write it as a single line.
async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &Value) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(frame).unwrap_or_default();
    buf.push(b'\n');
    writer.write_all(&buf).await?;
    writer.flush().await
}

/// Translate a bus event into an MCP notification frame. `None` for
/// events we don't expose (keeps the wire quiet and forward-compatible).
pub(crate) fn event_to_notification(event: &Event) -> Option<Value> {
    match event {
        Event::Sip(SipEvent::DialogCreated { call_id, .. }) => Some(json!({
            "jsonrpc": "2.0",
            "method": "notifications/call/created",
            "params": { "call_id": call_id },
        })),
        Event::Sip(SipEvent::DialogTerminated { call_id }) => Some(json!({
            "jsonrpc": "2.0",
            "method": "notifications/call/terminated",
            "params": { "call_id": call_id },
        })),
        Event::Plugin(PluginEvent::Notification {
            plugin,
            method,
            params,
        }) => Some(json!({
            "jsonrpc": "2.0",
            "method":  format!("notifications/plugin/{method}"),
            "params": {
                "plugin": plugin,
                // Forward the plugin's params verbatim so structured
                // fields (call_id, text, confidence, is_final, ...)
                // stay typed for the client.
                "data": params.clone().unwrap_or(Value::Null),
            },
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ControlState;
    use crate::jsonrpc::{ERR_METHOD_NOT_FOUND, ERR_PARSE};
    use crate::tool::test_support::{default_config, empty_registry, null_media};
    use smiths_core::{EventBus, RateLimitConfig};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, DuplexStream};

    fn dispatch(bus: &EventBus, cancel: &CancellationToken) -> ProtocolDispatch {
        let (state, _task) = ControlState::spawn(bus, cancel.clone());
        let ctx = ToolContext::new(state, empty_registry(), default_config(), null_media());
        ProtocolDispatch::new(
            Arc::new(crate::tools::builtin_registry()),
            Arc::new(crate::resource::builtin_registry()),
            Arc::new(RateLimiter::new(&RateLimitConfig::default())),
            Metrics::noop(),
            ctx,
        )
    }

    async fn handle_frame(frame: &str) -> Option<Value> {
        let bus = EventBus::new(8);
        let cancel = CancellationToken::new();
        let d = dispatch(&bus, &cancel);
        d.handle_frame(ACTOR, None, frame).await
    }

    /// Spawn `run_io` over an in-memory duplex pair. Returns the
    /// client's write half + a line reader over the server's output.
    struct Harness {
        to_server: DuplexStream,
        from_server: BufReader<DuplexStream>,
        bus: EventBus,
        cancel: CancellationToken,
        task: tokio::task::JoinHandle<std::io::Result<()>>,
    }

    impl Harness {
        fn spawn() -> Self {
            let bus = EventBus::new(16);
            let cancel = CancellationToken::new();
            let d = dispatch(&bus, &cancel);
            let (client_in, server_in) = tokio::io::duplex(64 * 1024);
            let (server_out, client_out) = tokio::io::duplex(64 * 1024);
            let task = tokio::spawn(run_io(
                server_in,
                server_out,
                d,
                bus.clone(),
                cancel.clone(),
            ));
            Self {
                to_server: client_in,
                from_server: BufReader::new(client_out),
                bus,
                cancel,
                task,
            }
        }

        async fn send(&mut self, line: &str) {
            self.to_server.write_all(line.as_bytes()).await.unwrap();
            self.to_server.write_all(b"\n").await.unwrap();
        }

        async fn recv(&mut self) -> Value {
            let mut line = String::new();
            tokio::time::timeout(
                Duration::from_secs(5),
                self.from_server.read_line(&mut line),
            )
            .await
            .expect("server reply within 5 s")
            .unwrap();
            serde_json::from_str(line.trim()).unwrap()
        }
    }

    #[test]
    fn negotiation_echoes_supported_and_falls_back_otherwise() {
        assert_eq!(negotiate_protocol_version(Some("2024-11-05")), "2024-11-05");
        assert_eq!(negotiate_protocol_version(Some("2025-06-18")), "2025-06-18");
        assert_eq!(
            negotiate_protocol_version(Some("1999-01-01")),
            PROTOCOL_VERSION
        );
        assert_eq!(negotiate_protocol_version(None), PROTOCOL_VERSION);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn initialize_returns_server_info_and_negotiated_version() {
        let frame = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#;
        let resp = handle_frame(frame).await.unwrap();
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["serverInfo"]["name"], "smiths-net");
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
        let resp = handle_frame(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .await
            .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tools_list_contains_builtins() {
        let resp = handle_frame(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .await
            .unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"list_calls"));
        assert!(names.contains(&"get_call_status"));
        assert!(names.contains(&"health"));
        assert!(names.contains(&"bridge_calls"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tools_call_health_ok() {
        let frame = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"health","arguments":{}}}"#;
        let resp = handle_frame(frame).await.unwrap();
        assert_eq!(resp["result"]["isError"], false);
        assert_eq!(resp["result"]["structuredContent"]["status"], "ok");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_method_returns_error() {
        let resp = handle_frame(r#"{"jsonrpc":"2.0","id":4,"method":"does/not/exist"}"#)
            .await
            .unwrap();
        assert_eq!(resp["error"]["code"], ERR_METHOD_NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn notification_produces_no_response() {
        let resp = handle_frame(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await;
        assert!(resp.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resources_list_contains_builtins() {
        let resp = handle_frame(r#"{"jsonrpc":"2.0","id":5,"method":"resources/list"}"#)
            .await
            .unwrap();
        let uris: Vec<&str> = resp["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["uri"].as_str().unwrap())
            .collect();
        assert!(uris.contains(&"health://status"));
        assert!(uris.contains(&"sip://calls"));
        assert!(uris.contains(&"config://current"));
        assert!(uris.contains(&"cluster://status"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resources_read_health_returns_status_ok() {
        let frame = r#"{"jsonrpc":"2.0","id":6,"method":"resources/read","params":{"uri":"health://status"}}"#;
        let resp = handle_frame(frame).await.unwrap();
        let body = resp["result"]["contents"][0]["text"].as_str().unwrap();
        let v: Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["status"], "ok");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resources_read_unknown_uri_is_error() {
        let frame =
            r#"{"jsonrpc":"2.0","id":7,"method":"resources/read","params":{"uri":"no://such"}}"#;
        let resp = handle_frame(frame).await.unwrap();
        assert_eq!(resp["error"]["code"], ERR_METHOD_NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_io_answers_requests_pushes_notifications_and_stops_at_eof() {
        let mut h = Harness::spawn();
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#)
            .await;
        let init = h.recv().await;
        assert_eq!(init["id"], 1);
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");

        // Blank lines and notifications are silently consumed.
        h.send("").await;
        h.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"health","arguments":{}}}"#)
            .await;
        let health = h.recv().await;
        assert_eq!(health["id"], 2);
        assert_eq!(health["result"]["structuredContent"]["status"], "ok");

        // Bus events surface as notifications on the same stream.
        h.bus
            .publish(Event::Sip(SipEvent::DialogCreated {
                call_id: "stdio-1@example".into(),
                media_endpoint: None,
                remote_rtp: None,
            }))
            .unwrap();
        let notif = h.recv().await;
        assert_eq!(notif["method"], "notifications/call/created");
        assert_eq!(notif["params"]["call_id"], "stdio-1@example");
        assert!(notif.get("id").is_none());

        // Malformed JSON gets a parse error with a null id.
        h.send("{oops").await;
        let parse = h.recv().await;
        assert_eq!(parse["error"]["code"], ERR_PARSE);
        assert!(parse["id"].is_null());

        // EOF on the input stops the loop cleanly.
        drop(h.to_server);
        let result = tokio::time::timeout(Duration::from_secs(5), h.task)
            .await
            .expect("server exits on EOF")
            .unwrap();
        assert!(result.is_ok());
        h.cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_io_rejects_oversize_frames_and_keeps_serving() {
        let mut h = Harness::spawn();
        // One frame well past the cap, streamed in chunks so the
        // reader has to discard across several fill_buf rounds.
        let oversize = "x".repeat(MAX_FRAME_BYTES + 1024);
        h.to_server.write_all(oversize.as_bytes()).await.unwrap();
        h.to_server.write_all(b"\n").await.unwrap();
        let err = h.recv().await;
        assert_eq!(err["error"]["code"], ERR_INVALID_REQUEST);
        assert!(
            err["error"]["message"]
                .as_str()
                .unwrap()
                .contains("exceeds"),
            "{err}"
        );
        // The next, well-formed frame is served normally.
        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#).await;
        let pong = h.recv().await;
        assert_eq!(pong["id"], 3);
        h.cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), h.task).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_io_stops_on_cancel() {
        let h = Harness::spawn();
        h.cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), h.task)
            .await
            .expect("server exits on cancel")
            .unwrap();
        assert!(result.is_ok());
    }
}
