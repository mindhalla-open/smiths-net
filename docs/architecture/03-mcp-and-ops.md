# MCP Server & Operations

## MCP server

Embedded in the binary. Three wire transports, all backed by the same
`ToolRegistry` + `ResourceRegistry` so behavior is identical across
adapters.

### Transports

| Transport       | Endpoint / wire                              | When to use                                                                                      |
|-----------------|----------------------------------------------|--------------------------------------------------------------------------------------------------|
| **MCP stdio**   | Line-delimited JSON-RPC 2.0 on stdin/stdout | Default for in-process AI agents (Claude Code, Cursor). CLI `--mcp stdio` suppresses SIP + health HTTP. |
| **MCP HTTP**    | `POST /mcp` (JSON-RPC 2.0 over HTTP)        | Remote agents or multi-agent deployments. Shares `dispatch` + audit + rate-limit paths with stdio. |
| **MCP SSE**     | `GET /mcp/events` (server-sent events)      | Streaming bus-originated notifications (`call/created`, `call/terminated`). 15 s keep-alive pings. |
| **A2A HTTP**    | `POST /a2a` + `GET /.well-known/agent.json` | Agent-to-agent integrations that expect the A2A protocol; same tool surface, bearer-token gated. |

HTTP + SSE + A2A all run alongside the SIP stack on the CLI-configured
bind; the `mcp-http` feature and the `[mcp] enabled_http / http_bind`
config toggle them on.

### Tools (shipped in-box)

Every tool is served identically by every adapter above; register a new
one once and it appears everywhere.

| Tool                 | Purpose                                                                             |
|----------------------|-------------------------------------------------------------------------------------|
| `list_calls`         | List dialogs the engine knows about, filterable by lifecycle phase.                 |
| `get_call_status`    | Fetch a single `CallSnapshot` by Call-ID.                                           |
| `health`             | Uptime + live-call count. Cheap liveness probe.                                     |
| `list_ai_providers`  | Loaded AI plugins with declared capabilities (filterable by capability name).       |
| `describe_provider`  | Full capability descriptor(s) for one plugin.                                       |
| `synthesize`         | Invoke an `ai.tts` plugin; returns base64 PCM16 LE bytes + format metadata.         |
| `transcribe`         | Invoke an `ai.asr` plugin on a base64-encoded PCM16 buffer. Returns transcript.     |
| `llm_chat`           | One-shot `ai.llm.chat` invocation with a messages array.                            |
| `embed`              | Parallel-indexed embeddings from an `ai.embed` plugin.                              |
| `speak`              | `synthesize` + real-time RTP streaming into a live call's media leg.                |
| `make_call`          | Place an outbound INVITE via the engine UAC. Returns Call-ID of the dialog.         |
| `end_call`           | Send BYE on a UAC-originated dialog.                                                |
| `reload_plugin`      | Drain + re-spawn one AI plugin from disk (re-runs `describe_capabilities`).         |

### Resources

| Resource URI      | Content                                                           |
|-------------------|-------------------------------------------------------------------|
| `health://status` | Uptime, known calls, live calls (mirrors the `health` tool).      |
| `sip://calls`     | Full snapshot of active + recently-terminated dialogs.            |
| `config://current`| Effective runtime config (secrets redacted).                      |

Load-time hot-add: `ResourceRegistry` accepts late registration, so
plugin-provided resources are a future wire without a schema change.

### Notifications (SSE)

`GET /mcp/events` emits a `text/event-stream`. Each line is a JSON
object mirroring an MCP notification the server would have produced
over stdio:

- `notifications/call/created` — on `SipEvent::DialogCreated`.
- `notifications/call/terminated` — on `SipEvent::DialogTerminated`.
- `notifications/plugin/{method}` — every plugin-initiated
  notification, with `{plugin, data}` params (from the plugin's
  bidirectional RPC surface).

Every connection gets a 15 s keep-alive ping so corporate proxies
don't drop idle streams.

### Security

- **Bearer auth** (A2A only by default; MCP HTTP is assumed to be on a
  trusted bind). `a2a.bearer_token` config; `/health` and
  `/.well-known/agent.json` remain public. 401 with `WWW-Authenticate`
  on failed auth.
- **Per-tool token-bucket rate limit** (`[mcp.rate_limit] per_sec, burst`;
  `0` disables). Shared between stdio, HTTP, and A2A so a single caller
  can't burst through a different adapter.
- **Audit log** — every invocation produces a structured `tracing` event
  at target `smiths_mcp::audit`: `actor`, `tool`, `args_hash` (SHA-256
  of the arguments, not the content), `outcome`, `duration_ms`, `error`.
  `actor` distinguishes `mcp-stdio` / `mcp-http` / `a2a`.

## Configuration

TOML with env overrides (`SMITHS__SECTION__KEY=...`).

```toml
[core]
worker_threads = 0              # 0 = num_cpus

[sip]
bind       = ["0.0.0.0:5060"]
transports = ["udp", "tcp"]

[sip.tls]
enabled = false
cert    = "/etc/smiths/cert.pem"
key     = "/etc/smiths/key.pem"
port    = 5061

[media]
rtp_port_range = [16384, 32767]
codecs         = ["PCMU", "PCMA", "opus"]

[plugins]
dir                = "/etc/smiths/plugins"
autoload           = ["rust-logger"]
require_signature  = false

[mcp]
enabled    = true
transport  = "stdio"            # "stdio" | "http"
http_bind  = "127.0.0.1:7878"
auth_token = "${MCP_TOKEN}"

[observability]
log_format   = "json"            # "json" | "pretty"
log_level    = "info"
metrics_bind = "127.0.0.1:9090"
```

**Hot-reloadable**: bind addresses (drain + rebind), codecs, plugin list,
log level, rate limits.
**Restart-required**: `worker_threads`, TLS key material (in v1; hot-reload
in v2).

Config loader uses `figment` layering: defaults → `/etc/smiths/config.toml`
→ `--config` file → env overrides → CLI flags.

## Observability

**Tracing**
- `tracing` + `tracing-subscriber` with JSON output by default.
- Spans correlate by `call_id` and `transaction_id`.
- Sidecar stdout/stderr is re-emitted with a `plugin=<name>` tag.

**Metrics** (Prometheus, exported on `[observability].metrics_bind`)
- `smiths_active_calls` (gauge)
- `smiths_sip_transactions_total{method,status}` (counter)
- `smiths_rtp_packets_total{direction,codec}` (counter)
- `smiths_rtp_bytes_total{direction,codec}` (counter)
- `smiths_plugin_hook_duration_seconds{plugin,hook}` (histogram)
- `smiths_plugin_errors_total{plugin,kind}` (counter)
- `smiths_mcp_tool_calls_total{tool,result}` (counter)

**Health**
- HTTP: `GET /health` (when HTTP transport is enabled).
- MCP resource: `health://status`.
- Checks: event bus liveness, wasmtime engine responsive, each supervised
  sidecar reports recent pong, no deadlock watchdog timeout.

**pcap tap**: per-call, opt-in. Feature-gated by `pcap`. Writes to a file
under a configurable directory, one pcap per call.

## Deployment

- **Binary**: `cargo build --release --target x86_64-unknown-linux-musl`,
  strip. Target < 15 MB. Dynamic targets (gnu) also supported.
- **Docker**: `FROM scratch` + binary + CA certs + default config.
  Alternatively `distroless` if pcap or TLS client cert stores are needed.
- **systemd**: unit runs as unprivileged user; grants `CAP_NET_BIND_SERVICE`
  if SIP port < 1024. Config/plugins mounted read-only.
- **Kubernetes**: Deployment + ConfigMap + headless Service. Each pod is an
  independent engine. Scale horizontally by fronting with a SIP load
  balancer or DNS SRV. No in-cluster session replication in v1.

## Upgrade / rollback

- Plugins: hot reload via MCP tool — zero downtime.
- Engine binary: drain-on-SIGTERM (configurable grace, default 30 s) then
  exit. Orchestrator does rolling restart. For zero-call-drop engine
  upgrades, pair with an external SIP load balancer that respects drain.

## Failure modes & guarantees

| Failure                       | Effect                                             |
|-------------------------------|----------------------------------------------------|
| Plugin WASM trap              | Call continues; plugin output for that invocation discarded; metric++ |
| Plugin sidecar crash          | Supervisor restarts per policy; in-flight plugin calls on that sidecar fail |
| SIP parse error               | Malformed message rejected; 400 Bad Request if origin is resolvable |
| RTP packet malformed          | Packet dropped; jitter/loss metrics updated        |
| Event bus backlog             | Per-subsystem bounded channels; drop-oldest policy for non-critical events; block for SIP transactions |
| Config hot-reload invalid     | Reload rejected; existing config preserved; error reported via MCP + log |
