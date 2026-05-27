# Phase 5 — MCP Server

**Goal**: LLM agents (Claude Code, Cursor, custom) drive the engine over
MCP. All tools and resources from `architecture/03-mcp-and-ops.md` work.
Authenticated, rate-limited, audited.

## Deliverables

1. `smiths-mcp` crate with:
   - `stdio` transport (JSON-RPC framing per MCP spec).
   - `http+sse` transport behind feature `mcp-http`.
   - Tool handlers for all tools in `03-mcp-and-ops.md §Tools`.
   - Resource handlers for all resources in that doc.
   - Bearer-token auth, per-tool rate limit, audit logging.
2. `smiths-cli` flag `--mcp` that switches the binary into MCP-stdio mode
   (so Claude Code can spawn it directly).
3. MCP integration tests driven by a small test client.

## Step-by-step tasks

1. **Transport layer**
   - stdio: JSON-RPC 2.0 frames per MCP spec. Use `rmcp` crate if stable;
     fallback to hand-rolled if it lags the spec.
   - http+sse (feature `mcp-http`): `axum` router, SSE for server→client,
     HTTP POST for client→server.
2. **Tool registry**
   - `Tool` trait: `name`, `input_schema`, `handle(args) → Result<Value>`.
   - Implementations for: `make_call`, `end_call`, `get_call_status`,
     `list_calls`, `list_plugins`, `load_plugin`, `unload_plugin`,
     `reload_plugin`, `set_plugin_config`, `start_capture`, `stop_capture`.
   - Each tool converts its input JSON into a `ControlEvent` and awaits
     a correlated reply on the bus.
3. **Resource registry**
   - `Resource` trait: `uri`, `read() → Value | Text`.
   - Implementations for: `sip://calls`, `sip://calls/{id}`,
     `plugin://manifests/{name}`, `config://current`,
     `metrics://snapshot`, `health://status`.
4. **Auth + rate limit**
   - Bearer token compare (`subtle` for constant-time).
   - Per-tool token bucket: default 30 req/min, configurable per tool.
   - Denials return `403` (HTTP) or JSON-RPC error `-32001`.
5. **Audit log**
   - Every tool call emits a `tracing::info!` event with `actor`, `tool`,
     `args_hash` (SHA-256 of JSON args, truncated), `result`, `latency_ms`.
   - Args themselves are not logged — hash only. Raw args kept at `debug`.
6. **CLI integration**
   - `smiths-cli --mcp stdio` enters MCP-stdio mode, no other servers
     bind.
   - Regular mode starts HTTP MCP if configured.
7. **Integration tests**
   - `mcp_tool_make_call.rs` — test client calls `make_call` to a test
     UAS; engine places call; UAS answers; client calls `end_call`.
   - `mcp_auth.rs` — wrong token is rejected; correct token works.
   - `mcp_rate_limit.rs` — 31st call in a minute is rejected.
   - `mcp_resource_calls.rs` — `sip://calls/{id}` returns expected JSON
     during a live call.

## Acceptance criteria

- [ ] Claude Code launched with the binary as an MCP server can call
  `make_call`, `list_plugins`, and read `sip://calls`.
- [ ] All tool/resource schemas are valid per MCP spec (checked with the
  official validator if available, else a local JSON-schema test).
- [ ] Auth failure paths produce correct error codes.
- [ ] Rate limits enforced; rejected calls are audited.
- [ ] HTTP/SSE transport works with two concurrent clients.

## Out of scope

- Multi-tenant MCP (per-agent isolation) — backlog.
- OAuth flow for MCP — backlog.
- Schema generation from Rust types (nice-to-have).

## Risks & notes

- MCP spec moves. Pin the spec version in `docs/architecture/03-mcp-and-ops.md`
  and bump deliberately.
- Do not route MCP calls through plugins unless explicitly designed for
  that — tools hit the event bus directly.
- Treat `make_call`'s `destination` as untrusted: validate with the SIP
  URI parser before any routing.
